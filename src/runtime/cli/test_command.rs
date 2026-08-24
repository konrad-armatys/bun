use bun_io::Write as _;
use core::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::cli::Command;
use crate::cli::test::changed_files_filter as ChangedFilesFilter;
use crate::cli::test::parallel_runner as ParallelRunner;
use crate::cli::test::scanner::{self, Scanner};
use crate::cli::test::timings::Timings;
use bun_collections::BoundedArray;
use bun_core::{self as bun, Global, Output, env_var, fmt as bun_fmt};
use bun_core::{EncodedSlice, strings};
use bun_core::{pretty_error, pretty_errorln};
use bun_dotenv as DotEnv;
use bun_jsc::virtual_machine::VirtualMachine;
use bun_jsc::{self as jsc};
use bun_options_types::code_coverage_options::CodeCoverageOptions;
use bun_paths::resolve_path;
use bun_paths::string_paths::without_leading_path_separator;
use bun_paths::{self as bun_path, PathBuffer};
use bun_ptr::Interned;
use bun_resolver::fs::FileSystem;
use bun_sys::{self, Fd, File};

// Debug log scope for test-runner entrypoint loading.
bun_output::declare_scope!(bun_test, hidden);

// ─── coverage façade ────────────────────────────────────────────────────────
// Thin adapter over `bun_sourcemap_jsc::code_coverage` that preserves the
// legacy call paths used in `print_code_coverage` below (the adapter
// dispatches the runtime `enable_ansi_colors` bool to the const generic).
// Drop once the body is normalised to call `code_coverage::{text,lcov}`
// directly with `<ENABLE_ANSI_COLORS>`.
mod coverage {
    pub(super) use bun_sourcemap_jsc::code_coverage::{
        ByteRangeMapping, Fraction, Report as CodeCoverageReport, lcov as Lcov,
    };

    /// Less-than predicate adapted to the `Ordering` shape `sort_by` wants.
    #[inline]
    pub(super) fn is_less_than_cmp(
        a: &&mut ByteRangeMapping,
        b: &&mut ByteRangeMapping,
    ) -> core::cmp::Ordering {
        bun_core::order(a.source_url.slice(), b.source_url.slice())
    }

    #[allow(non_snake_case)]
    pub(super) mod Text {
        use super::*;
        use bun_sourcemap_jsc::code_coverage::text;

        /// Runtime-bool → const-generic dispatch for `text::write_format`.
        #[inline]
        pub(crate) fn write_format(
            report: &CodeCoverageReport,
            max_filename_length: usize,
            fraction: &mut Fraction,
            base_path: &[u8],
            writer: &mut impl bun_io::Write,
            enable_ansi_colors: bool,
        ) -> bun_io::Result<()> {
            if enable_ansi_colors {
                text::write_format::<true>(report, max_filename_length, fraction, base_path, writer)
            } else {
                text::write_format::<false>(
                    report,
                    max_filename_length,
                    fraction,
                    base_path,
                    writer,
                )
            }
        }

        /// Runtime-bool → const-generic dispatch for `text::write_format_with_values`.
        #[inline]
        pub(crate) fn write_format_with_values(
            filename: &[u8],
            max_filename_length: usize,
            vals: Fraction,
            failing: Fraction,
            failed: bool,
            writer: &mut impl bun_io::Write,
            indent_name: bool,
            enable_ansi_colors: bool,
        ) -> bun_io::Result<()> {
            if enable_ansi_colors {
                text::write_format_with_values::<true>(
                    filename,
                    max_filename_length,
                    vals,
                    failing,
                    failed,
                    writer,
                    indent_name,
                )
            } else {
                text::write_format_with_values::<false>(
                    filename,
                    max_filename_length,
                    vals,
                    failing,
                    failed,
                    writer,
                    indent_name,
                )
            }
        }
    }
}
use coverage::{ByteRangeMapping, CodeCoverageReport, Fraction};

// ─── compat shim: map legacy paths onto the test_runner crate ────────────────
// The body was originally written against `bun_jsc::jest::{bun_test, Snapshots,
// TestRunner}` before `crate::test_runner` existed. Those types now live under
// `crate::test_runner::*`; the façade below adapts the body's nested-path
// usage (`bun_test::Execution::Result`, `bun_test::BasicResult`, …) without a
// 2k-line body rewrite.
use crate::test_runner::jest::{self, Summary, TestRunner};
use crate::test_runner::snapshot::Snapshots;
use bun_collections::index_sort;

#[allow(non_snake_case)]
mod bun_test {
    //! Façade over `crate::test_runner` that preserves the legacy paths
    //! the body uses (`bun_test::Execution::Result`, `bun_test::BasicResult`,
    //! `bun_test::DescribeScope`, …). Drop once the body is normalised.

    /// `add_result()` queue payload.
    pub(super) use crate::test_runner::bun_test::RefDataValue as ResultMsg;
    pub(super) use crate::test_runner::bun_test::*;
    pub(super) use crate::test_runner::execution::{
        Basic as BasicResult, ExpectAssertions, PendingIs as PendingMode,
    };
    #[allow(non_snake_case)]
    pub(super) mod Execution {
        pub(crate) use crate::test_runner::execution::*;
    }
}

pub(crate) fn escape_xml(str_: &[u8], writer: &mut impl bun_io::Write) -> crate::Result<()> {
    let mut last: usize = 0;
    let mut i: usize = 0;
    let len = str_.len();
    while i < len {
        let c = str_[i];
        match c {
            b'&' | b'<' | b'>' | b'"' | b'\'' => {
                if i > last {
                    writer.write_all(&str_[last..i])?;
                }
                writer.write_all(bun_core::strings::xml_escape_entity(c).unwrap())?;
                last = i + 1;
            }
            b'\t' | b'\n' | b'\r' => {
                // Valid XML 1.0 Char. Emit as a numeric reference so the literal
                // byte survives attribute-value normalisation (XML 1.0 §3.3.3).
                if i > last {
                    writer.write_all(&str_[last..i])?;
                }
                write!(writer, "&#{};", c)?;
                last = i + 1;
            }
            0..=0x1f => {
                // Any other C0 control character is not a valid XML 1.0 Char and
                // cannot be represented even as a numeric reference, so drop it.
                if i > last {
                    writer.write_all(&str_[last..i])?;
                }
                last = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if len > last {
        writer.write_all(&str_[last..])?;
    }
    Ok(())
}

fn fmt_status_text_line(
    status: bun_test::Execution::Result,
    emoji_or_color: bool,
) -> Output::PrettyBuf {
    // emoji and color might be split into two different options in the future
    // some terminals support color, but not emoji.
    // For now, they are the same.
    match emoji_or_color {
        true => match status.basic_result() {
            bun_test::BasicResult::Pending => Output::pretty_fmt::<true>("<r><d>…<r>"),
            bun_test::BasicResult::Pass => Output::pretty_fmt::<true>("<r><green>✓<r>"),
            bun_test::BasicResult::Fail => Output::pretty_fmt::<true>("<r><red>✗<r>"),
            bun_test::BasicResult::Skip => Output::pretty_fmt::<true>("<r><yellow>»<d>"),
            bun_test::BasicResult::Todo => Output::pretty_fmt::<true>("<r><magenta>✎<r>"),
        },
        false => match status.basic_result() {
            bun_test::BasicResult::Pending => Output::pretty_fmt::<false>("<r><d>(pending)<r>"),
            bun_test::BasicResult::Pass => Output::pretty_fmt::<false>("<r><green>(pass)<r>"),
            bun_test::BasicResult::Fail => Output::pretty_fmt::<false>("<r><red>(fail)<r>"),
            bun_test::BasicResult::Skip => Output::pretty_fmt::<false>("<r><yellow>(skip)<d>"),
            bun_test::BasicResult::Todo => Output::pretty_fmt::<false>("<r><magenta>(todo)<r>"),
        },
    }
}

// `Output::error_writer()` / `Output::writer()` already return an unbounded
// `&mut io::Writer`; the previous local `err_w`/`out_w` wrappers were no-op
// reborrows. Call sites use the `Output` accessors directly.

#[derive(Default)]
pub struct JunitFailure {
    pub name: Vec<u8>,
    pub(crate) message: Vec<u8>,
    pub(crate) body: Vec<u8>,
}

/// Append `input` to `out`, dropping CSI sequences (`ESC '[' ... final`), so a
/// matcher message built with colour does not reach the report as SGR residue.
fn push_stripping_ansi(out: &mut Vec<u8>, input: &[u8]) {
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'[' {
            i += 2;
            while i < input.len() && !(0x40..=0x7e).contains(&input[i]) {
                i += 1;
            }
            if i < input.len() {
                i += 1;
            }
            continue;
        }
        out.push(input[i]);
        i += 1;
    }
}

// Remaining TODOs:
// - Add stdout/stderr to the JUnit report
// - Add timestamp field to the JUnit report
#[derive(Default)]
pub struct JunitReporter {
    pub(crate) contents: Vec<u8>,
    pub(crate) total_metrics: Metrics,
    pub(crate) offset_of_testsuites_value: usize,
    pub(crate) current_file: Box<[u8]>,
    pub(crate) sent_upto: usize,
    pub(crate) elements_only: bool,
    pub(crate) file_start_ns: u64,
    pub(crate) file_end_ns: u64,
    pub(crate) properties_list_to_repeat_in_every_test_suite: Option<Box<[u8]>>,

    pub(crate) suite_stack: Vec<SuiteInfo>,
    pub(crate) current_depth: u32,

    /// Error captured by `on_uncaught_exception` for the currently-failing
    /// test; consumed by `write_test_case` on the next `Result::Fail`.
    pub(crate) last_failure: Option<JunitFailure>,

    pub(crate) hostname_value: Option<Box<[u8]>>,
}

#[derive(Default)]
pub struct SuiteInfo {
    pub name: Box<[u8]>,
    pub(crate) offset_of_attributes: usize,
    pub(crate) metrics: Metrics,
    pub(crate) is_file_suite: bool,
    pub(crate) started_ns: u64,
}

// We dupe the name unconditionally in begin_test_suite_with_line, so the
// unconditional drop is correct.

#[derive(Default, Clone, Copy)]
pub struct Metrics {
    pub(crate) test_cases: u32,
    pub(crate) assertions: u32,
    pub(crate) failures: u32,
    pub(crate) skipped: u32,
    pub(crate) elapsed_time: u64,
}

impl Metrics {
    fn add(&mut self, other: &Metrics) {
        self.test_cases += other.test_cases;
        self.assertions += other.assertions;
        self.failures += other.failures;
        self.skipped += other.skipped;
    }
}

impl JunitReporter {
    pub(crate) fn get_hostname(&mut self) -> Option<&[u8]> {
        if self.hostname_value.is_none() {
            #[cfg(windows)]
            {
                return None;
            }

            #[cfg(not(windows))]
            {
                const HOST_NAME_MAX: usize = 256;
                let mut name_buffer = [0u8; HOST_NAME_MAX];
                if bun_sys::posix::gethostname(&mut name_buffer).is_err() {
                    self.hostname_value = Some(Box::default());
                    return None;
                }
                let hostname = bun_core::slice_to_nul(&name_buffer);

                let mut arraylist_writer: Vec<u8> = Vec::new();
                if escape_xml(hostname, &mut arraylist_writer).is_err() {
                    self.hostname_value = Some(Box::default());
                    return None;
                }
                self.hostname_value = Some(arraylist_writer.into_boxed_slice());
            }
        }

        if let Some(hostname) = &self.hostname_value {
            if !hostname.is_empty() {
                return Some(hostname);
            }
        }
        None
    }

    pub(crate) fn init() -> Box<JunitReporter> {
        Box::new(JunitReporter::default())
    }

    // `pub const new = bun.TrivialNew(JunitReporter);` → Box::new

    /// Capture name/message/stack from the `ZigException` that
    /// `print_error_instance_body` has already populated, so the next
    /// `write_test_case` can emit a useful `<failure>` without re-running
    /// the exception formatter.
    pub(crate) fn record_failure(&mut self, exception: &jsc::ZigException) {
        let failure = self.last_failure.get_or_insert_default();
        let name = exception.name.to_utf8();
        let raw_message = exception.message.to_utf8();
        let mut message = Vec::with_capacity(raw_message.slice().len());
        push_stripping_ansi(&mut message, raw_message.slice());

        let is_assertion = strings::has_prefix_comptime(&message, b"expect(")
            && (name.slice().is_empty() || strings::eql(name.slice(), b"Error"));

        if failure.name.is_empty() {
            if is_assertion {
                failure.name.extend_from_slice(b"AssertionError");
            } else {
                failure.name.extend_from_slice(name.slice());
            }
        }
        if failure.message.is_empty() {
            failure.message.extend_from_slice(&message);
        }

        let body = &mut failure.body;
        if !body.is_empty() {
            body.push(b'\n');
        }
        let header: &[u8] = if is_assertion {
            b"AssertionError"
        } else {
            name.slice()
        };
        match (header.is_empty(), message.is_empty()) {
            (true, true) => body.extend_from_slice(b"error"),
            (true, false) => body.extend_from_slice(&message),
            (false, true) => body.extend_from_slice(header),
            (false, false) => {
                body.extend_from_slice(header);
                body.extend_from_slice(b": ");
                body.extend_from_slice(&message);
            }
        }
        body.push(b'\n');
        let dir = FileSystem::instance().top_level_dir;
        for frame in exception.stack.frames() {
            let source_url = frame.source_url.to_utf8();
            let file = jsc::ZigStackFrame::relative_source_url(dir, source_url.slice());
            let func = frame.function_name.to_utf8();
            if file.is_empty() && func.slice().is_empty() {
                continue;
            }
            body.extend_from_slice(b"      at ");
            if !func.slice().is_empty() {
                let _ = write!(body, "{} (", frame.name_formatter(false));
            }
            let file_start = body.len();
            body.extend_from_slice(file);
            if cfg!(windows) {
                for b in &mut body[file_start..] {
                    if *b == b'\\' {
                        *b = b'/';
                    }
                }
            }
            let pos = frame.position;
            if pos.line.is_valid() && pos.column.is_valid() {
                let _ = write!(body, ":{}:{}", pos.line.one_based(), pos.column.one_based());
            } else if pos.line.is_valid() {
                let _ = write!(body, ":{}", pos.line.one_based());
            }
            if !func.slice().is_empty() {
                body.push(b')');
            }
            body.push(b'\n');
        }
    }

    /// VirtualMachine::on_print_error_zig_exception hook body.
    pub(crate) fn record_failure_cb(reporter: &CommandLineReporter, exception: &jsc::ZigException) {
        if let Some(this) = reporter.reporters.junit.borrow_mut().as_deref_mut() {
            this.record_failure(exception);
        }
    }

    fn generate_properties_list(&mut self) -> crate::Result<()> {
        struct PropertiesList<'a> {
            ci: &'a [u8],
            commit: &'a [u8],
        }

        let ci_buf: Vec<u8>;
        let ci: &[u8] = 'brk: {
            if let Some(github_run_id) = env_var::GITHUB_RUN_ID.get() {
                if let Some(github_server_url) = env_var::GITHUB_SERVER_URL.get() {
                    if let Some(github_repository) = env_var::GITHUB_REPOSITORY.get() {
                        if !github_run_id.is_empty()
                            && !github_server_url.is_empty()
                            && !github_repository.is_empty()
                        {
                            let mut v = Vec::new();
                            // Std::io::Write removed; bun_io::Write (top-level) provides write_fmt.
                            let _ = write!(
                                &mut v,
                                "{}/{}/actions/runs/{}",
                                bstr::BStr::new(github_server_url),
                                bstr::BStr::new(github_repository),
                                bstr::BStr::new(github_run_id)
                            );
                            ci_buf = v;
                            break 'brk &ci_buf[..];
                        }
                    }
                }
            }

            if let Some(ci_job_url) = env_var::CI_JOB_URL.get() {
                if !ci_job_url.is_empty() {
                    break 'brk ci_job_url;
                }
            }

            break 'brk b"";
        };

        let commit: &[u8] = 'brk: {
            if let Some(github_sha) = env_var::GITHUB_SHA.get() {
                if !github_sha.is_empty() {
                    break 'brk github_sha;
                }
            }

            if let Some(sha) = env_var::CI_COMMIT_SHA.get() {
                if !sha.is_empty() {
                    break 'brk sha;
                }
            }

            if let Some(git_sha) = env_var::GIT_SHA.get() {
                if !git_sha.is_empty() {
                    break 'brk git_sha;
                }
            }

            break 'brk b"";
        };

        let properties = PropertiesList { ci, commit };

        if properties.ci.is_empty() && properties.commit.is_empty() {
            self.properties_list_to_repeat_in_every_test_suite = Some(Box::default());
            return Ok(());
        }

        let mut buffer: Vec<u8> = Vec::new();
        let writer = &mut buffer;

        writer.write_all(b"    <properties>\n")?;

        if !properties.ci.is_empty() {
            writer.write_all(b"      <property name=\"ci\" value=\"")?;
            escape_xml(properties.ci, writer)?;
            writer.write_all(b"\" />\n")?;
        }
        if !properties.commit.is_empty() {
            writer.write_all(b"      <property name=\"commit\" value=\"")?;
            escape_xml(properties.commit, writer)?;
            writer.write_all(b"\" />\n")?;
        }

        writer.write_all(b"    </properties>\n")?;

        self.properties_list_to_repeat_in_every_test_suite = Some(buffer.into_boxed_slice());
        Ok(())
    }

    fn get_indent(depth: u32) -> &'static [u8] {
        const SPACES: &[u8] =
            b"                                                                                ";
        const INDENT_SIZE: u32 = 2;
        let total_spaces = (depth + 1) * INDENT_SIZE;
        &SPACES[0..(total_spaces as usize).min(SPACES.len())]
    }

    pub(crate) fn begin_test_suite(&mut self, name: &[u8]) -> crate::Result<()> {
        self.begin_test_suite_with_line(name, 0, true)
    }

    pub(crate) fn begin_test_suite_with_line(
        &mut self,
        name: &[u8],
        line_number: u32,
        is_file_suite: bool,
    ) -> crate::Result<()> {
        if self.contents.is_empty() && !self.elements_only {
            self.contents
                .extend_from_slice(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
            self.contents
                .extend_from_slice(b"<testsuites name=\"bun test\" ");
            self.offset_of_testsuites_value = self.contents.len();
            self.contents.extend_from_slice(b">\n");
        }

        let indent = Self::get_indent(self.current_depth);
        self.contents.extend_from_slice(indent);
        self.contents.extend_from_slice(b"<testsuite name=\"");
        escape_xml(name, &mut self.contents)?;
        self.contents.extend_from_slice(b"\"");

        if is_file_suite {
            self.contents.extend_from_slice(b" file=\"");
            escape_xml(name, &mut self.contents)?;
            self.contents.extend_from_slice(b"\"");
        } else if !self.current_file.is_empty() {
            self.contents.extend_from_slice(b" file=\"");
            // Reshaped for borrowck — clone current_file slice before mutable borrow of contents
            let cf = self.current_file.clone();
            escape_xml(&cf, &mut self.contents)?;
            self.contents.extend_from_slice(b"\"");
        }

        if line_number > 0 {
            // Std::io::Write removed; bun_io::Write (top-level) provides write_fmt.
            let _ = write!(&mut self.contents, " line=\"{}\"", line_number);
        }

        self.contents.extend_from_slice(b" ");
        let offset_of_attributes = self.contents.len();
        self.contents.extend_from_slice(b">\n");

        if is_file_suite {
            if self.properties_list_to_repeat_in_every_test_suite.is_none() {
                self.generate_properties_list()?;
            }

            if let Some(properties_list) = &self.properties_list_to_repeat_in_every_test_suite {
                if !properties_list.is_empty() {
                    self.contents.extend_from_slice(properties_list);
                }
            }
        }

        self.suite_stack.push(SuiteInfo {
            name: Box::<[u8]>::from(name),
            offset_of_attributes,
            metrics: Metrics::default(),
            is_file_suite,
            started_ns: if is_file_suite { self.file_start_ns } else { 0 },
        });

        self.current_depth += 1;
        if is_file_suite {
            self.current_file = Box::<[u8]>::from(name);
        }
        Ok(())
    }

    pub(crate) fn end_test_suite(&mut self) -> crate::Result<()> {
        if self.suite_stack.is_empty() {
            return Ok(());
        }

        self.current_depth -= 1;
        let suite_info = self.suite_stack.swap_remove(self.suite_stack.len() - 1);

        let elapsed_time_seconds = if suite_info.is_file_suite && suite_info.started_ns > 0 {
            let end_ns = if self.file_end_ns >= suite_info.started_ns {
                self.file_end_ns
            } else {
                bun::Timespec::now(bun::TimespecMockMode::ForceRealTime).ns()
            };
            end_ns.saturating_sub(suite_info.started_ns) as f64 / bun::time::NS_PER_S as f64
        } else {
            suite_info.metrics.elapsed_time as f64 / bun::time::MS_PER_S as f64
        };

        // Reshaped for borrowck — get hostname first
        let hostname = self.get_hostname().map(|h| h.to_vec()).unwrap_or_default();

        // Insert the summary attributes
        let mut summary = Vec::new();
        {
            // Std::io::Write removed; bun_io::Write (top-level) provides write_fmt.
            let _ = write!(
                &mut summary,
                "tests=\"{}\" assertions=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{}\" hostname=\"{}\"",
                suite_info.metrics.test_cases,
                suite_info.metrics.assertions,
                suite_info.metrics.failures,
                suite_info.metrics.skipped,
                elapsed_time_seconds,
                bstr::BStr::new(&hostname),
            );
        }

        self.contents.splice(
            suite_info.offset_of_attributes..suite_info.offset_of_attributes,
            summary.iter().copied(),
        );

        let indent = Self::get_indent(self.current_depth);
        self.contents.extend_from_slice(indent);
        self.contents.extend_from_slice(b"</testsuite>\n");

        if !self.suite_stack.is_empty() {
            let last = self.suite_stack.len() - 1;
            self.suite_stack[last].metrics.add(&suite_info.metrics);
        } else {
            self.total_metrics.add(&suite_info.metrics);
        }
        Ok(())
    }

    pub(crate) fn write_test_case(
        &mut self,
        status: bun_test::Execution::Result,
        file: &[u8],
        name: &[u8],
        class_name: &[u8],
        assertions: u32,
        elapsed_ns: u64,
        line_number: u32,
    ) -> crate::Result<()> {
        // Std::io::Write removed; bun_io::Write (top-level) provides write_fmt.
        let elapsed_ns_f64: f64 = elapsed_ns as f64;
        let elapsed_ms = elapsed_ns_f64 / bun::time::NS_PER_MS as f64;

        if !self.suite_stack.is_empty() {
            let last = self.suite_stack.len() - 1;
            let current_suite = &mut self.suite_stack[last];
            current_suite.metrics.elapsed_time = current_suite
                .metrics
                .elapsed_time
                .saturating_add(elapsed_ms as u64);
            current_suite.metrics.test_cases += 1;
            current_suite.metrics.assertions += assertions;
        }

        let indent = Self::get_indent(self.current_depth);
        self.contents.extend_from_slice(indent);
        self.contents.extend_from_slice(b"<testcase");
        self.contents.extend_from_slice(b" name=\"");
        escape_xml(name, &mut self.contents)?;
        self.contents.extend_from_slice(b"\" classname=\"");
        escape_xml(class_name, &mut self.contents)?;
        self.contents.extend_from_slice(b"\"");

        let elapsed_seconds = elapsed_ms / bun::time::MS_PER_S as f64;
        let _ = write!(
            &mut self.contents,
            " time=\"{}\"",
            bun_fmt::trimmed_precision::<6>(elapsed_seconds)
        );

        self.contents.extend_from_slice(b" file=\"");
        escape_xml(file, &mut self.contents)?;
        self.contents.extend_from_slice(b"\"");

        if line_number > 0 {
            let _ = write!(&mut self.contents, " line=\"{}\"", line_number);
        }

        let _ = write!(&mut self.contents, " assertions=\"{}\"", assertions);

        use bun_test::Execution::Result as R;
        match status {
            R::Pass => {
                self.contents.extend_from_slice(b" />\n");
            }
            R::Fail => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.failures += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                let failure = self.last_failure.take();
                let type_name: &[u8] = failure
                    .as_ref()
                    .map(|f| f.name.as_slice())
                    .filter(|n| !n.is_empty())
                    .unwrap_or(b"Error");
                self.contents.extend_from_slice(b"  <failure type=\"");
                escape_xml(type_name, &mut self.contents)?;
                self.contents.extend_from_slice(b"\"");
                if let Some(f) = failure.as_ref() {
                    if !f.message.is_empty() {
                        self.contents.extend_from_slice(b" message=\"");
                        escape_xml(&f.message, &mut self.contents)?;
                        self.contents.extend_from_slice(b"\"");
                    }
                }
                match failure.as_ref().filter(|f| !f.body.is_empty()) {
                    Some(f) => {
                        self.contents.extend_from_slice(b">");
                        escape_xml(&f.body, &mut self.contents)?;
                        self.contents.extend_from_slice(b"</failure>\n");
                    }
                    None => {
                        self.contents.extend_from_slice(b" />\n");
                    }
                }
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::FailBecauseFailingTestPassed => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.failures += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                let _ = writeln!(
                    &mut self.contents,
                    "  <failure message=\"test marked with .failing() did not throw\" type=\"AssertionError\"/>"
                );
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::FailBecauseExpectedAssertionCount => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.failures += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                let _ = writeln!(
                    &mut self.contents,
                    "  <failure message=\"Expected more assertions, but only received {}\" type=\"AssertionError\"/>",
                    assertions
                );
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::FailBecauseTodoPassed => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.failures += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                let _ = writeln!(
                    &mut self.contents,
                    "  <failure message=\"TODO passed\" type=\"AssertionError\"/>"
                );
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::FailBecauseExpectedHasAssertions => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.failures += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                let _ = writeln!(
                    &mut self.contents,
                    "  <failure message=\"Expected to have assertions, but none were run\" type=\"AssertionError\"/>"
                );
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::SkippedBecauseLabel | R::Skip => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.skipped += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"  <skipped />\n");
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::Todo => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.skipped += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                self.contents
                    .extend_from_slice(b"  <skipped message=\"TODO\" />\n");
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::FailBecauseTimeout
            | R::FailBecauseTimeoutWithDoneCallback
            | R::FailBecauseHookTimeout
            | R::FailBecauseHookTimeoutWithDoneCallback => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.failures += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(
                    b"  <failure type=\"TimeoutError\" message=\"test timed out\" />\n",
                );
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::Pending => unreachable!(),
        }
        self.last_failure = None;
        Ok(())
    }

    pub(crate) fn write_to_file(&mut self, path: &[u8]) -> crate::Result<()> {
        if self.contents.is_empty() {
            return Ok(());
        }

        while !self.suite_stack.is_empty() {
            self.end_test_suite()?;
        }

        {
            let metrics = self.total_metrics;
            let elapsed_time = (bun::time::nano_timestamp() - bun::start_time()) as f64
                / bun::time::NS_PER_S as f64;
            let mut summary = Vec::new();
            {
                // Std::io::Write removed; bun_io::Write (top-level) provides write_fmt.
                let _ = write!(
                    &mut summary,
                    "tests=\"{}\" assertions=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{}\"",
                    metrics.test_cases,
                    metrics.assertions,
                    metrics.failures,
                    metrics.skipped,
                    elapsed_time,
                );
            }
            self.contents.splice(
                self.offset_of_testsuites_value..self.offset_of_testsuites_value,
                summary.iter().copied(),
            );
            self.contents.extend_from_slice(b"</testsuites>\n");
        }

        let mut junit_path_buf = PathBuffer::uninit();

        junit_path_buf[..path.len()].copy_from_slice(path);
        junit_path_buf[path.len()] = 0;

        // SAFETY: junit_path_buf[path.len()] == 0 written above
        let zpath = bun_core::ZStr::from_buf(&junit_path_buf[..], path.len());
        match File::openat(
            Fd::cwd(),
            zpath,
            bun_sys::O::WRONLY | bun_sys::O::CREAT | bun_sys::O::TRUNC,
            0o664,
        ) {
            bun_sys::Result::Err(err) => {
                Output::err(
                    crate::Error::JUnitReportFailed,
                    "Failed to write JUnit report to {}\n{}",
                    (bstr::BStr::new(path), err),
                );
            }
            bun_sys::Result::Ok(fd) => match File::write_all(&fd, &self.contents) {
                bun_sys::Result::Ok(()) => {}
                bun_sys::Result::Err(err) => {
                    Output::err(
                        crate::Error::JUnitReportFailed,
                        "Failed to write JUnit report to {}\n{}",
                        (bstr::BStr::new(path), err),
                    );
                }
            },
        }
        Ok(())
    }
}

/// Drain the event loop after a file's tests finish, like a node process
/// would before exiting; the vendored-node-test runner opts in via
/// `BUN_TEST_DRAIN_EVENT_LOOP=1` so mustCall()-style exit checks see
/// completed async work. Off by default: bun suites keep exit-after-tests.
fn should_drain_event_loop() -> bool {
    env_var::BUN_TEST_DRAIN_EVENT_LOOP.get().unwrap_or(false)
}

/// jest and vitest never run a test file's `process.on('exit')` listeners; node's test harness asserts from them.
pub(crate) fn skip_exit_listeners(reporter: &CommandLineReporter) -> bool {
    !(reporter.jest.node_test_used.get() || should_drain_event_loop())
}

/// Leaked for the process lifetime by `TestCommand::exec` (which never returns
/// before process exit) and shared with JS-reentrant test-runner callbacks, so
/// it is `&self`-only: state written after construction is `Cell`/`RefCell`.
pub struct CommandLineReporter {
    pub(crate) jest: TestRunner,
    pub(crate) repeat_count: u32,
    pub(crate) last_printed_dot: Cell<bool>,

    /// When running as a `--parallel` worker, this is the coordinator-assigned
    /// index of the file currently being executed. While set, per-test output
    /// is sent over the IPC pipe instead of to stderr; the coordinator owns
    /// the terminal.
    pub(crate) worker_ipc_file_idx: Cell<Option<u32>>,

    pub(crate) failures_to_repeat_buf: RefCell<Vec<u8>>,
    pub(crate) skips_to_repeat_buf: RefCell<Vec<u8>>,
    pub(crate) todos_to_repeat_buf: RefCell<Vec<u8>>,

    pub(crate) reporters: ReportersConfig,

    /// `--timings`: loaded before the run, updated per file, written back under `--update-timings`.
    pub(crate) timings: RefCell<Option<Timings>>,
}

#[derive(Default)]
pub struct ReportersConfig {
    pub(crate) dots: bool,
    pub(crate) only_failures: bool,
    pub(crate) junit: RefCell<Option<Box<JunitReporter>>>,
}

impl CommandLineReporter {
    fn print_test_line<const DIM: bool>(
        status: bun_test::Execution::Result,
        sequence: &bun_test::Execution::ExecutionSequence,
        test_entry: &bun_test::ExecutionEntry,
        elapsed_ns: u64,
        writer: &mut impl bun_io::Write,
    ) {
        let initial_retry_count = test_entry.retry_count;
        let attempts = (initial_retry_count - sequence.remaining_retry_count.get()) + 1;
        let initial_repeat_count = test_entry.repeat_count;
        let repeats = (initial_repeat_count - sequence.remaining_repeat_count.get()) + 1;
        let mut scopes_stack: BoundedArray<Rc<bun_test::DescribeScope>, 64> =
            BoundedArray::default();
        let mut parent_: Option<Rc<bun_test::DescribeScope>> = test_entry.base.parent();

        while let Some(scope) = parent_ {
            parent_ = scope.base.parent();
            if scopes_stack.push(scope).is_err() {
                break;
            }
        }

        let scopes: &[Rc<bun_test::DescribeScope>] = scopes_stack.as_slice();
        let display_label: &[u8] = test_entry.base.name.as_deref().unwrap_or(b"(unnamed)");

        // Quieter output when claude code is in use.
        if !Output::is_ai_agent() || !status.is_pass(bun_test::PendingMode::PendingIsFail) {
            // `color_code`/`line_color_code` literals are inlined at use sites
            // below via `if DIM { ... } else { ... }` to avoid runtime `format!`.

            // `switch (Output.enable_ansi_colors_stderr) { inline else => |_| ... }` — the
            // captured bool was unused except for monomorphization; collapsed to runtime.
            match status {
                bun_test::Execution::Result::FailBecauseExpectedAssertionCount => {
                    // not sent to writer so it doesn't get printed twice
                    let expected_count = if let bun_test::ExpectAssertions::Exact(n) =
                        sequence.expect_assertions.get()
                    {
                        n
                    } else {
                        12345
                    };
                    Output::err(
                        crate::Error::AssertionError,
                        "expected <green>{} assertion{}<r>, but test ended with <red>{} assertion{}<r>\n",
                        (
                            expected_count,
                            if expected_count == 1 { "" } else { "s" },
                            sequence.expect_call_count.get(),
                            if sequence.expect_call_count.get() == 1 {
                                ""
                            } else {
                                "s"
                            },
                        ),
                    );
                    Output::flush();
                }
                bun_test::Execution::Result::FailBecauseExpectedHasAssertions => {
                    Output::err(
                        crate::Error::AssertionError,
                        "received <red>0 assertions<r>, but expected <green>at least one assertion<r> to be called\n",
                        (),
                    );
                    Output::flush();
                }
                bun_test::Execution::Result::FailBecauseTimeout
                | bun_test::Execution::Result::FailBecauseHookTimeout
                | bun_test::Execution::Result::FailBecauseTimeoutWithDoneCallback
                | bun_test::Execution::Result::FailBecauseHookTimeoutWithDoneCallback => {
                    if Output::is_github_action() {
                        Output::print_error(format_args!(
                            "::error title=error: Test \"{}\" timed out after {}ms::\n",
                            bun_fmt::github_action_property(display_label),
                            test_entry.timeout
                        ));
                        Output::flush();
                    }
                }
                _ => {}
            }

            if Output::enable_ansi_colors_stderr() {
                for i in 0..scopes.len() {
                    let index = (scopes.len() - 1) - i;
                    let scope = &scopes[index];
                    let name: &[u8] = scope.base.name.as_deref().unwrap_or(b"");
                    if name.is_empty() {
                        continue;
                    }
                    let _ = writer.write_all(b" ");

                    let prefix = if DIM {
                        Output::pretty_fmt::<true>("<r><d>")
                    } else {
                        Output::pretty_fmt::<true>("<r>")
                    };
                    let _ = writer.write_all(&prefix);
                    let _ = writer.write_all(name);
                    let _ = writer.write_all(&Output::pretty_fmt::<true>("<d>"));
                    let _ = writer.write_all(b" >");
                }
            } else {
                for i in 0..scopes.len() {
                    let index = (scopes.len() - 1) - i;
                    let scope = &scopes[index];
                    let name: &[u8] = scope.base.name.as_deref().unwrap_or(b"");
                    if name.is_empty() {
                        continue;
                    }
                    let _ = writer.write_all(b" ");
                    let _ = writer.write_all(name);
                    let _ = writer.write_all(b" >");
                }
            }

            if Output::enable_ansi_colors_stderr() {
                let label_prefix = if DIM {
                    Output::pretty_fmt::<true>("<r><d> ")
                } else {
                    Output::pretty_fmt::<true>("<r><b> ")
                };
                let _ = writer.write_all(&label_prefix);
                let _ = writer.write_all(display_label);
                let _ = writer.write_all(&Output::pretty_fmt::<true>("<r>"));
            } else {
                let _ = writer.write_all(b" ");
                let _ = writer.write_all(display_label);
            }

            // Print attempt count if test was retried (attempts > 1)
            if attempts > 1 {
                let _ = bun_core::write_pretty!(
                    writer,
                    Output::enable_ansi_colors_stderr(),
                    " <d>(attempt {d})<r>",
                    attempts,
                );
            }

            // Print repeat count if test failed on a repeat (repeats > 1)
            if repeats > 1 {
                let _ = bun_core::write_pretty!(
                    writer,
                    Output::enable_ansi_colors_stderr(),
                    " <d>(run {d})<r>",
                    repeats,
                );
            }

            if elapsed_ns > (bun::time::NS_PER_US * 10) {
                let _ = write!(
                    writer,
                    " {}",
                    Output::ElapsedFormatter {
                        colors: Output::enable_ansi_colors_stderr(),
                        duration_ns: elapsed_ns,
                    }
                );
            }

            let _ = writer.write_all(b"\n");

            let colors = Output::enable_ansi_colors_stderr();
            use bun_test::Execution::Result as R;
            match status {
                R::Pending | R::Pass | R::Skip | R::SkippedBecauseLabel | R::Todo | R::Fail => {}

                R::FailBecauseFailingTestPassed => {
                    let _ = bun_core::write_pretty!(
                        writer,
                        colors,
                        "  <d>^<r> <red>this test is marked as failing but it passed.<r> <d>Remove `.failing` if tested behavior now works<r>\n"
                    );
                }
                R::FailBecauseTodoPassed => {
                    let _ = bun_core::write_pretty!(
                        writer,
                        colors,
                        "  <d>^<r> <red>this test is marked as todo but passes.<r> <d>Remove `.todo` if tested behavior now works<r>\n"
                    );
                }
                R::FailBecauseExpectedAssertionCount | R::FailBecauseExpectedHasAssertions => {} // printed above
                R::FailBecauseTimeout => {
                    let _ = bun_core::write_pretty!(
                        writer,
                        colors,
                        "  <d>^<r> <red>this test timed out after {d}ms.<r>\n",
                        test_entry.timeout
                    );
                }
                R::FailBecauseHookTimeout => {
                    let _ = bun_core::write_pretty!(
                        writer,
                        colors,
                        "  <d>^<r> <red>a beforeEach/afterEach hook timed out for this test.<r>\n"
                    );
                }
                R::FailBecauseTimeoutWithDoneCallback => {
                    let _ = bun_core::write_pretty!(
                        writer,
                        colors,
                        "  <d>^<r> <red>this test timed out after {d}ms, before its done callback was called.<r> <d>If a done callback was not intended, remove the last parameter from the test callback function<r>\n",
                        test_entry.timeout
                    );
                }
                R::FailBecauseHookTimeoutWithDoneCallback => {
                    let _ = bun_core::write_pretty!(
                        writer,
                        colors,
                        "  <d>^<r> <red>a beforeEach/afterEach hook timed out before its done callback was called.<r> <d>If a done callback was not intended, remove the last parameter from the hook callback function<r>\n"
                    );
                }
            }
        }
    }

    fn maybe_print_junit_line(
        status: bun_test::Execution::Result,
        buntest: &bun_test::BunTest,
        sequence: &bun_test::Execution::ExecutionSequence,
        test_entry: &bun_test::ExecutionEntry,
        elapsed_ns: u64,
    ) {
        let Some(cmd_reporter) = buntest.reporter.get() else {
            return;
        };
        let mut junit_slot = cmd_reporter.reporters.junit.borrow_mut();
        let Some(junit) = junit_slot.as_deref_mut() else {
            return;
        };

        let mut scopes_stack: BoundedArray<Rc<bun_test::DescribeScope>, 64> =
            BoundedArray::default();
        let mut parent_: Option<Rc<bun_test::DescribeScope>> = test_entry.base.parent();
        let assertions = sequence.expect_call_count.get();
        let line_number = test_entry.base.line_no;

        let file: &[u8] = if let Some(runner) = jest::Jest::runner() {
            runner.file_path(buntest.file_id).text
        } else {
            b""
        };

        while let Some(scope) = parent_ {
            parent_ = scope.base.parent();
            if scopes_stack.push(scope).is_err() {
                break;
            }
        }

        let scopes: &[Rc<bun_test::DescribeScope>] = scopes_stack.as_slice();
        let display_label: &[u8] = test_entry.base.name.as_deref().unwrap_or(b"(unnamed)");

        {
            let filename: &[u8] = 'brk: {
                let top = FileSystem::instance().top_level_dir;
                if strings::has_prefix(file, top) {
                    break 'brk without_leading_path_separator(&file[top.len()..]);
                } else {
                    break 'brk file;
                }
            };

            if !strings::eql(&junit.current_file, filename) {
                while !junit.suite_stack.is_empty()
                    && !junit.suite_stack[junit.suite_stack.len() - 1].is_file_suite
                {
                    junit.end_test_suite().expect("oom");
                }

                if !junit.current_file.is_empty() {
                    junit.end_test_suite().expect("oom");
                }

                junit.begin_test_suite(filename).expect("oom");
            }

            // To make the juint reporter generate nested suites, we need to find the needed suites and create/print them.
            // This assumes that the scopes are in the correct order.
            let mut needed_suites: Vec<&bun_test::DescribeScope> = Vec::new();

            for i in 0..scopes.len() {
                let index = (scopes.len() - 1) - i;
                let scope = &*scopes[index];
                if let Some(name) = scope.base.name.as_deref() {
                    if !name.is_empty() {
                        needed_suites.push(scope);
                    }
                }
            }

            let mut current_suite_depth: u32 = 0;
            if !junit.suite_stack.is_empty() {
                for suite_info in &junit.suite_stack {
                    if !suite_info.is_file_suite {
                        current_suite_depth += 1;
                    }
                }
            }

            while (current_suite_depth as usize) > needed_suites.len() {
                if !junit.suite_stack.is_empty()
                    && !junit.suite_stack[junit.suite_stack.len() - 1].is_file_suite
                {
                    junit.end_test_suite().expect("oom");
                    current_suite_depth -= 1;
                } else {
                    break;
                }
            }

            let mut suites_to_close: u32 = 0;
            let mut suite_index: usize = 0;
            for suite_info in &junit.suite_stack {
                if suite_info.is_file_suite {
                    continue;
                }

                if suite_index < needed_suites.len() {
                    let needed_scope = needed_suites[suite_index];
                    let needed_name = needed_scope.base.name.as_deref().unwrap_or(b"");
                    if !strings::eql(&suite_info.name, needed_name) {
                        suites_to_close = current_suite_depth - u32::try_from(suite_index).unwrap();
                        break;
                    }
                } else {
                    suites_to_close = current_suite_depth - u32::try_from(suite_index).unwrap();
                    break;
                }
                suite_index += 1;
            }

            while suites_to_close > 0 {
                if !junit.suite_stack.is_empty()
                    && !junit.suite_stack[junit.suite_stack.len() - 1].is_file_suite
                {
                    junit.end_test_suite().expect("oom");
                    suites_to_close -= 1;
                } else {
                    break;
                }
            }

            let mut describe_suite_index: usize = 0;
            for suite_info in &junit.suite_stack {
                if !suite_info.is_file_suite {
                    describe_suite_index += 1;
                }
            }

            while describe_suite_index < needed_suites.len() {
                let scope = needed_suites[describe_suite_index];
                let (name, line_no) = (
                    scope.base.name.as_deref().unwrap_or(b""),
                    scope.base.line_no,
                );
                junit
                    .begin_test_suite_with_line(name, line_no, false)
                    .expect("oom");
                describe_suite_index += 1;
            }

            let mut concatenated_describe_scopes: Vec<u8> = Vec::new();

            {
                let initial_length = concatenated_describe_scopes.len();
                for scope in scopes {
                    if let Some(name) = scope.base.name.as_deref() {
                        if !name.is_empty() {
                            if initial_length != concatenated_describe_scopes.len() {
                                concatenated_describe_scopes.extend_from_slice(b" > ");
                            }

                            // write_test_case escapes class_name once; do not pre-escape here.
                            concatenated_describe_scopes.extend_from_slice(name);
                        }
                    }
                }
            }

            junit
                .write_test_case(
                    status,
                    filename,
                    display_label,
                    &concatenated_describe_scopes,
                    assertions,
                    elapsed_ns,
                    line_number,
                )
                .expect("oom");
        }
    }

    #[inline]
    pub(crate) fn summary(&self) -> core::cell::RefMut<'_, Summary> {
        self.jest.summary.borrow_mut()
    }

    pub(crate) fn handle_test_completed(
        buntest: &bun_test::BunTest,
        sequence: &bun_test::Execution::ExecutionSequence,
        test_entry: &bun_test::ExecutionEntry,
        elapsed_ns: u64,
    ) {
        let mut output_buf: Vec<u8> = Vec::new();

        let initial_length = output_buf.len();
        let writer = &mut output_buf;

        let result = sequence.result.get();
        if result != bun_test::Execution::Result::SkippedBecauseLabel {
            let reporter_ref: Option<&CommandLineReporter> = buntest.reporter.get();
            let basic = result.basic_result();
            let dots_branch = reporter_ref.is_some_and(|r| r.reporters.dots)
                && matches!(
                    basic,
                    bun_test::BasicResult::Pass
                        | bun_test::BasicResult::Skip
                        | bun_test::BasicResult::Todo
                        | bun_test::BasicResult::Pending
                );
            if dots_branch {
                let colors = Output::enable_ansi_colors_stderr();
                match basic {
                    bun_test::BasicResult::Pass => {
                        let _ = bun_core::write_pretty!(writer, colors, "<r><green>.<r>");
                    }
                    bun_test::BasicResult::Skip => {
                        let _ = bun_core::write_pretty!(writer, colors, "<r><yellow>.<d>");
                    }
                    bun_test::BasicResult::Todo => {
                        let _ = bun_core::write_pretty!(writer, colors, "<r><magenta>.<r>");
                    }
                    bun_test::BasicResult::Pending => {
                        let _ = bun_core::write_pretty!(writer, colors, "<r><d>.<r>");
                    }
                    bun_test::BasicResult::Fail => {
                        let _ = bun_core::write_pretty!(writer, colors, "<r><red>.<r>");
                    }
                }
                reporter_ref.unwrap().last_printed_dot.set(true);
            } else if basic != bun_test::BasicResult::Fail
                && reporter_ref.is_some_and(|r| r.reporters.only_failures)
            {
                // when using --only-failures, only print failures
            } else {
                buntest.bun_test_root.on_before_print();

                if Output::enable_ansi_colors_stderr() {
                    let _ = writer.write_all(&fmt_status_text_line(result, true));
                } else {
                    let _ = writer.write_all(&fmt_status_text_line(result, false));
                }
                let dim = match basic {
                    bun_test::BasicResult::Todo => {
                        if let Some(runner) = jest::Jest::runner() {
                            !runner.run_todo
                        } else {
                            true
                        }
                    }
                    bun_test::BasicResult::Skip | bun_test::BasicResult::Pending => true,
                    bun_test::BasicResult::Pass | bun_test::BasicResult::Fail => false,
                };
                if dim {
                    Self::print_test_line::<true>(result, sequence, test_entry, elapsed_ns, writer);
                } else {
                    Self::print_test_line::<false>(
                        result, sequence, test_entry, elapsed_ns, writer,
                    );
                }
            }
        }
        // always print junit if needed
        Self::maybe_print_junit_line(result, buntest, sequence, test_entry, elapsed_ns);

        let formatted_line = &output_buf[initial_length..];
        let worker_idx = buntest
            .reporter
            .get()
            .and_then(|r| r.worker_ipc_file_idx.get());
        if let Some(idx) = worker_idx {
            ParallelRunner::worker_emit_test_done(idx, formatted_line);
        } else {
            let _ = Output::error_writer().write_all(formatted_line);
        }

        let Some(this) = buntest.reporter.get() else {
            return;
        }; // command line reporter is missing! uh oh!

        if !this.reporters.dots && !this.reporters.only_failures {
            match sequence.result.get().basic_result() {
                bun_test::BasicResult::Skip => this
                    .skips_to_repeat_buf
                    .borrow_mut()
                    .extend_from_slice(&output_buf[initial_length..]),
                bun_test::BasicResult::Todo => this
                    .todos_to_repeat_buf
                    .borrow_mut()
                    .extend_from_slice(&output_buf[initial_length..]),
                bun_test::BasicResult::Fail => this
                    .failures_to_repeat_buf
                    .borrow_mut()
                    .extend_from_slice(&output_buf[initial_length..]),
                bun_test::BasicResult::Pass | bun_test::BasicResult::Pending => {}
            }
        }

        use bun_test::Execution::Result as R;
        match sequence.result.get() {
            R::Pending => {}
            R::Pass => this.summary().pass += 1,
            R::Skip => this.summary().skip += 1,
            R::Todo => this.summary().todo += 1,
            R::SkippedBecauseLabel => this.summary().skipped_because_label += 1,

            R::Fail
            | R::FailBecauseFailingTestPassed
            | R::FailBecauseTodoPassed
            | R::FailBecauseExpectedHasAssertions
            | R::FailBecauseExpectedAssertionCount
            | R::FailBecauseTimeout
            | R::FailBecauseTimeoutWithDoneCallback
            | R::FailBecauseHookTimeout
            | R::FailBecauseHookTimeoutWithDoneCallback => {
                this.summary().fail += 1;

                if this.summary().fail == this.jest.bail {
                    this.print_summary();
                    pretty_error!(
                        "\nBailed out after {} failure{}<r>\n",
                        this.jest.bail,
                        if this.jest.bail == 1 { "" } else { "s" }
                    );
                    Output::flush();
                    this.write_junit_report_if_needed();
                    this.write_timings_if_needed();
                    Global::exit(1);
                }
            }
        }
        {
            let mut summary = this.summary();
            summary.expectations = summary
                .expectations
                .saturating_add(sequence.expect_call_count.get());
        }
    }

    pub(crate) fn print_summary(&self) {
        let summary_ = *self.summary();
        let tests = summary_.fail + summary_.pass + summary_.skip + summary_.todo;
        let files = summary_.files;

        pretty_error!(
            "Ran {} test{} across {} file{}. ",
            tests,
            if tests == 1 { "" } else { "s" },
            files,
            if files == 1 { "" } else { "s" }
        );

        Output::print_start_end(bun::start_time(), bun::time::nano_timestamp());
    }

    /// Like the JUnit report, called before every exit path (including bail) so measured durations aren't lost.
    pub(crate) fn write_timings_if_needed(&self) {
        if self.jest.test_options.update_timings
            && self.worker_ipc_file_idx.get().is_none()
            && let Some(timings) = self.timings.borrow_mut().as_mut()
        {
            timings.write(self.jest.test_options.shard.is_some());
        }
    }

    /// Writes the JUnit reporter output file if a JUnit reporter is active and
    /// an outfile path was configured. This must be called before any early exit
    /// (e.g. bail) so that the report is not lost.
    pub(crate) fn write_junit_report_if_needed(&self) {
        if let Some(junit) = self.reporters.junit.borrow_mut().as_mut() {
            if let Some(outfile) = self.jest.test_options.reporter_outfile.as_deref() {
                if !junit.current_file.is_empty() {
                    let _ = junit.end_test_suite();
                }
                let _ = junit.write_to_file(outfile);
            }
        }
    }

    pub(crate) fn generate_code_coverage(
        &self,
        vm: &mut VirtualMachine,
        opts: &mut CodeCoverageOptions,
        reporters_text: bool,
        reporters_lcov: bool,
        enable_ansi_colors: bool,
    ) -> crate::Result<()> {
        if !reporters_text && !reporters_lcov {
            return Ok(());
        }

        ByteRangeMapping::with_map(|map| {
            // `ByteRangeMapping` owns a `MultiArrayList` and is not `Copy`, so
            // collect mutable borrows into the thread-local map instead — no
            // double-free risk.
            let mut byte_ranges: Vec<&mut ByteRangeMapping> = map.values_mut().collect();
            if byte_ranges.is_empty() {
                return Ok(());
            }

            index_sort::sort_slice_by(&mut byte_ranges, coverage::is_less_than_cmp);

            self.print_code_coverage(
                vm,
                opts,
                &mut byte_ranges,
                reporters_text,
                reporters_lcov,
                enable_ansi_colors,
            )
        })
        .unwrap_or(Ok(()))
    }

    pub(crate) fn render_lcov(
        &self,
        vm: &mut VirtualMachine,
        opts: &CodeCoverageOptions,
    ) -> Option<Vec<u8>> {
        ByteRangeMapping::with_map(|map| {
            // See `generate_code_coverage` — collect borrows, not bitwise copies.
            let mut byte_ranges: Vec<&mut ByteRangeMapping> = map.values_mut().collect();
            if byte_ranges.is_empty() {
                return None;
            }
            index_sort::sort_slice_by(&mut byte_ranges, coverage::is_less_than_cmp);
            Self::render_lcov_ranges(vm, opts, &mut byte_ranges)
        })
        .flatten()
    }

    fn render_lcov_ranges(
        vm: &mut VirtualMachine,
        opts: &CodeCoverageOptions,
        byte_ranges: &mut [&mut ByteRangeMapping],
    ) -> Option<Vec<u8>> {
        let relative_dir = bun_resolver::fs::FileSystem::get().top_level_dir;
        let mut buffered: Vec<u8> = Vec::with_capacity(64 * 1024);
        let writer = &mut buffered;

        for entry in byte_ranges.iter_mut() {
            if !opts.ignore_patterns.is_empty() {
                let rel = resolve_path::relative(relative_dir, entry.source_url.slice());
                let mut skip = false;
                for p in &opts.ignore_patterns {
                    if bun_glob::r#match(p, rel).matches() {
                        skip = true;
                        break;
                    }
                }
                if skip {
                    continue;
                }
            }
            let Some(report) =
                CodeCoverageReport::generate(vm.global(), entry, opts.ignore_sourcemap)
            else {
                continue;
            };
            // report dropped at end of iteration
            if coverage::Lcov::write_format(&report, relative_dir, writer).is_err() {
                continue;
            }
            drop(report);
        }
        Some(buffered)
    }

    pub(crate) fn print_code_coverage(
        &self,
        vm: &mut VirtualMachine,
        opts: &mut CodeCoverageOptions,
        byte_ranges: &mut [&mut ByteRangeMapping],
        reporters_text: bool,
        reporters_lcov: bool,
        enable_ansi_colors: bool,
    ) -> crate::Result<()> {
        // Both spellings are compile-time constants; pick one by the runtime flag.
        macro_rules! pretty_lit {
            ($fmt:literal) => {
                if enable_ansi_colors {
                    bun_core::pretty_fmt!($fmt, true).as_bytes()
                } else {
                    bun_core::pretty_fmt!($fmt, false).as_bytes()
                }
            };
        }
        // `perf::Ctx` ends its span on Drop.
        let _trace = if reporters_text && reporters_lcov {
            bun::perf::trace("TestCommand.printCodeCoverageLCovAndText")
        } else if reporters_text {
            bun::perf::trace("TestCommand.printCodeCoverageText")
        } else if reporters_lcov {
            bun::perf::trace("TestCommand.printCodeCoverageLCov")
        } else {
            // Unreachable by construction.
            unreachable!("No reporters enabled")
        };

        if !reporters_text && !reporters_lcov {
            unreachable!("No reporters enabled");
        }

        let relative_dir = bun_resolver::fs::FileSystem::get().top_level_dir;

        // --- Text ---
        let max_filepath_length: usize = if reporters_text {
            'brk: {
                let mut len = b"All files".len();
                for entry in byte_ranges.iter() {
                    let utf8 = entry.source_url.slice();
                    let relative_path = resolve_path::relative(relative_dir, utf8);

                    // Check if this file should be ignored based on coveragePathIgnorePatterns
                    if !opts.ignore_patterns.is_empty() {
                        let mut should_ignore = false;
                        for pattern in &opts.ignore_patterns {
                            if bun_glob::r#match(pattern, relative_path).matches() {
                                should_ignore = true;
                                break;
                            }
                        }

                        if should_ignore {
                            continue;
                        }
                    }

                    len = relative_path.len().max(len);
                }

                break 'brk len;
            }
        } else {
            0
        };

        // `&mut bun_core::io::Writer: bun_io::Write` (impl in `bun_core::io`);
        // `splat_byte_all` / `write_all` resolve via the trait import at top.
        let mut console = Output::error_writer();
        let base_fraction = opts.fractions;
        let mut failing = false;

        if reporters_text {
            if console.write_all(pretty_lit!("<r><d>")).is_err() {
                return Ok(());
            }
            if console
                .splat_byte_all(b'-', max_filepath_length + 2)
                .is_err()
            {
                return Ok(());
            }
            if console
                .write_all(pretty_lit!("|---------|---------|-------------------<r>\n"))
                .is_err()
            {
                return Ok(());
            }
            if console.write_all(b"File").is_err() {
                return Ok(());
            }
            if console
                .splat_byte_all(b' ', max_filepath_length - b"File".len() + 1)
                .is_err()
            {
                return Ok(());
            }
            if console
                .write_all(pretty_lit!(
                    " <d>|<r> % Funcs <d>|<r> % Lines <d>|<r> Uncovered Line #s\n"
                ))
                .is_err()
            {
                return Ok(());
            }
            if console.write_all(pretty_lit!("<d>")).is_err() {
                return Ok(());
            }
            if console
                .splat_byte_all(b'-', max_filepath_length + 2)
                .is_err()
            {
                return Ok(());
            }
            if console
                .write_all(pretty_lit!("|---------|---------|-------------------<r>\n"))
                .is_err()
            {
                return Ok(());
            }
        }

        let mut console_buffer: Vec<u8> = Vec::new();
        let console_writer = &mut console_buffer;

        let mut avg = Fraction {
            functions: 0.0,
            lines: 0.0,
            stmts: 0.0,
            ..Default::default()
        };
        let mut avg_count: f64 = 0.0;
        // --- Text ---

        // --- LCOV ---
        let mut lcov_name_buf = PathBuffer::uninit();
        let mut lcov_state: Option<(File, &bun_core::ZStr, /*buffered*/ Vec<u8>)> =
            if reporters_lcov {
                'brk: {
                    // Ensure the directory exists
                    let mut fs = crate::node::fs::NodeFS::default();
                    let _ = fs.mkdir_recursive(&crate::node::fs::args::Mkdir {
                        path: crate::node::PathLike::borrowed(&opts.reports_directory),
                        always_return_none: true,
                        recursive: true,
                        ..Default::default()
                    });

                    // Write the lcov.info file to a temporary file we atomically rename to the final name after it succeeds
                    let mut base64_bytes = [0u8; 8];
                    let mut shortname_buf = [0u8; 512];
                    bun_boringssl_sys::rand_bytes(&mut base64_bytes);
                    // Temp name: `.lcov.info.<lowercase hex of 8 random bytes>.tmp`.
                    let tmpname = {
                        use std::io::Write as _;
                        let mut cursor = &mut shortname_buf[..];
                        let _ = cursor.write_all(b".lcov.info.");
                        let _ = write!(cursor, "{}", bun_core::fmt::hex_lower(&base64_bytes));
                        let _ = cursor.write_all(b".tmp\0");
                        let s = bun_core::slice_to_nul(&shortname_buf);
                        // NUL written above; `slice_to_nul` returns the prefix before it.
                        bun_core::ZStr::from_buf(&shortname_buf[..], s.len())
                    };
                    let path = resolve_path::join_abs_string_buf_z::<bun_path::platform::Auto>(
                        relative_dir,
                        &mut lcov_name_buf,
                        &[&opts.reports_directory, tmpname.as_bytes()],
                    );
                    let file = File::openat(
                        Fd::cwd(),
                        path,
                        bun_sys::O::CREAT
                            | bun_sys::O::WRONLY
                            | bun_sys::O::TRUNC
                            | bun_sys::O::CLOEXEC,
                        0o644,
                    );

                    match file {
                        bun_sys::Result::Err(err) => {
                            Output::err(
                                crate::Error::lcovCoverageError,
                                "Failed to create lcov file",
                                (),
                            );
                            Output::print_error(format_args!("\n{}", err));
                            Global::exit(1);
                        }
                        bun_sys::Result::Ok(f) => {
                            // Accumulate in a `Vec<u8>` (impl `bun_io::Write`)
                            // and flush to the fd via `write_all` on success
                            // below.
                            let buffered: Vec<u8> = Vec::with_capacity(64 * 1024);
                            break 'brk Some((f, path, buffered));
                        }
                    }
                }
            } else {
                None
            };
        let mut lcov_guard = scopeguard::guard(
            &mut lcov_state,
            |s: &mut Option<(File, &bun_core::ZStr, Vec<u8>)>| {
                if reporters_lcov {
                    if let Some((file, name, _)) = s.take() {
                        let _ = file.close(); // close error is non-actionable
                        let _ = bun_sys::unlink(name);
                    }
                }
            },
        );
        // --- LCOV ---

        for entry in byte_ranges.iter_mut() {
            // Check if this file should be ignored based on coveragePathIgnorePatterns
            if !opts.ignore_patterns.is_empty() {
                let utf8 = entry.source_url.slice();
                let relative_path = resolve_path::relative(relative_dir, utf8);

                let mut should_ignore = false;
                for pattern in &opts.ignore_patterns {
                    if bun_glob::r#match(pattern, relative_path).matches() {
                        should_ignore = true;
                        break;
                    }
                }

                if should_ignore {
                    continue;
                }
            }

            let Some(report) =
                CodeCoverageReport::generate(vm.global(), entry, opts.ignore_sourcemap)
            else {
                continue;
            };

            if reporters_text {
                let mut fraction = base_fraction;
                if coverage::Text::write_format(
                    &report,
                    max_filepath_length,
                    &mut fraction,
                    relative_dir,
                    console_writer,
                    enable_ansi_colors,
                )
                .is_err()
                {
                    continue;
                }
                avg.functions += fraction.functions;
                avg.lines += fraction.lines;
                avg.stmts += fraction.stmts;
                avg_count += 1.0;
                if fraction.failing {
                    failing = true;
                }

                console_writer.extend_from_slice(b"\n");
            }

            if reporters_lcov {
                if let Some((_, _, buffered)) = lcov_guard.as_mut() {
                    if coverage::Lcov::write_format(&report, relative_dir, buffered).is_err() {
                        continue;
                    }
                }
            }

            drop(report);
        }

        if reporters_text {
            {
                if avg_count == 0.0 {
                    avg.functions = 0.0;
                    avg.lines = 0.0;
                    avg.stmts = 0.0;
                } else {
                    avg.functions /= avg_count;
                    avg.lines /= avg_count;
                    avg.stmts /= avg_count;
                }

                let failed = if avg_count > 0.0 {
                    base_fraction
                } else {
                    Fraction {
                        functions: 0.0,
                        lines: 0.0,
                        stmts: 0.0,
                        ..Default::default()
                    }
                };

                coverage::Text::write_format_with_values(
                    b"All files",
                    max_filepath_length,
                    avg,
                    failed,
                    failing,
                    &mut console,
                    false,
                    enable_ansi_colors,
                )?;

                console.write_all(pretty_lit!("<r><d> |<r>\n"))?;
            }

            console.write_all(&console_buffer)?;
            console.write_all(pretty_lit!("<r><d>"))?;
            // Disarm the lcov cleanup guard before the early `Ok(())`; the
            // temp file is left for the OS.
            if console
                .splat_byte_all(b'-', max_filepath_length + 2)
                .is_err()
            {
                let _ = scopeguard::ScopeGuard::into_inner(lcov_guard);
                return Ok(());
            }
            if console
                .write_all(pretty_lit!("|---------|---------|-------------------<r>\n"))
                .is_err()
            {
                let _ = scopeguard::ScopeGuard::into_inner(lcov_guard);
                return Ok(());
            }

            opts.fractions.failing = failing;
            Output::flush();
        }

        if reporters_lcov {
            // `try lcov_writer.flush()` — keep the errdefer guard armed across the
            // write so an error here still closes + unlinks the temp file.
            if let Some((lcov_file, _, buffered)) = &mut **lcov_guard {
                if let bun_sys::Result::Err(e) = lcov_file.write_all(buffered) {
                    // `lcov_guard` drops on this early return → close + unlink.
                    return Err(crate::Error::from(e));
                }
            }
            // Flush succeeded — disarm the errdefer cleanup.
            let state = scopeguard::ScopeGuard::into_inner(lcov_guard);
            if let Some((lcov_file, lcov_name, _)) = state.take() {
                let _ = lcov_file.close();
                let cwd = Fd::cwd();
                if let Err(err) = bun_sys::move_file_z(
                    cwd,
                    lcov_name,
                    cwd,
                    resolve_path::join_abs_string_z::<bun_path::platform::Auto>(
                        relative_dir,
                        &[&opts.reports_directory, b"lcov.info"],
                    ),
                ) {
                    Output::err(err, "Failed to save lcov.info file", ());
                    Global::exit(1);
                }
            }
        } else {
            let _ = scopeguard::ScopeGuard::into_inner(lcov_guard);
        }
        Ok(())
    }
}

// HOST_EXPORT(BunTest__shouldGenerateCodeCoverage, c)
pub fn should_generate_code_coverage(test_name_str: &bun_core::String) -> bool {
    let zig_slice = test_name_str.to_utf8();
    // In this particular case, we don't actually care about non-ascii latin1 characters.
    // so we skip the ascii check
    let slice: &[u8] = zig_slice.slice();

    // always ignore node_modules.
    if strings::contains(slice, b"/node_modules/") || strings::contains(slice, b"\\node_modules\\")
    {
        return false;
    }

    let ext = bun_path::extension(slice);
    // SAFETY: `VirtualMachine::get()` returns the process-lifetime VM pointer; only
    // called from the JS thread once a VM exists.
    let loader_by_ext = VirtualMachine::get()
        .as_mut()
        .transpiler
        .options
        .loader(ext);

    // allow file loader just incase they use a custom loader with a non-standard extension
    if !(loader_by_ext.is_javascript_like() || loader_by_ext == bun_ast::Loader::File) {
        return false;
    }

    if let Some(runner) = jest::Jest::runner() {
        if runner.test_options.coverage.skip_test_files {
            let name_without_extension = &slice[0..slice.len() - ext.len()];
            for suffix in scanner::TEST_NAME_SUFFIXES {
                if strings::ends_with(name_without_extension, suffix) {
                    return false;
                }
            }
        }
    }

    true
}

pub(crate) struct TestCommand;

impl TestCommand {
    // pub use bun_options_types::code_coverage_options::{CodeCoverageOptions, Reporter, Reporters};
    // Re-exports moved to top-level `use` per crate map.

    pub(crate) fn exec(ctx: Command::Context) -> crate::Result<()> {
        Output::IS_GITHUB_ACTION.store(
            Output::is_github_action(),
            core::sync::atomic::Ordering::Relaxed,
        );

        if !ctx.test_options.test_worker {
            // print the version so you know its doing stuff if it takes a sec
            let w = Output::writer();
            let colors = Output::enable_ansi_colors_stdout();
            let _ = w.write_all(&if colors {
                Output::pretty_fmt::<true>(const_format::concatcp!(
                    "<r><b>bun test <r><d>v",
                    Global::package_json_version_with_sha,
                    "<r>"
                ))
            } else {
                Output::pretty_fmt::<false>(const_format::concatcp!(
                    "<r><b>bun test <r><d>v",
                    Global::package_json_version_with_sha,
                    "<r>"
                ))
            });
            if ctx.test_options.parallel > 0 {
                if colors {
                    let _ = write!(
                        w,
                        " \x1b[1;2m{}\u{00d7} PARALLEL\x1b[0m",
                        ctx.test_options.parallel
                    );
                } else {
                    let _ = write!(w, " {}x PARALLEL", ctx.test_options.parallel);
                }
            }
            let _ = w.write_all(b"\n");
            Output::flush();
        }

        // `exec()` never returns before process exit, so the heap allocation
        // outlives all observers.
        let mut env_loader: Box<DotEnv::Loader> = Box::new(DotEnv::Loader::init());
        jsc::initialize(jsc::InitializeOptions {
            short_lived_globals: ctx.test_options.isolate,
            ..Default::default()
        });
        bun_http::http_thread::init(&Default::default());

        let enable_random = ctx.test_options.randomize;
        let seed: u32 = if enable_random {
            ctx.test_options
                .seed
                .unwrap_or_else(|| bun::fast_random() as u32) // @truncate
        } else {
            0
        }; // seed is limited to u32 so storing it in js doesn't lose precision
        // Persist the chosen seed so --parallel forwards it to every worker;
        // otherwise each worker would draw its own and the printed --seed=N
        // would not reproduce the run.
        if enable_random {
            ctx.test_options.seed = Some(seed);
        }
        // `DefaultPrng` is `Copy`, so pass the prng by value to TestRunner
        // and keep a local copy for shuffling.
        let random_instance: Option<bun::rand::DefaultPrng> = if enable_random {
            Some(bun::rand::DefaultPrng::init(seed as u64))
        } else {
            None
        };

        jsc::virtual_machine::isBunTest.store(true, core::sync::atomic::Ordering::Relaxed);

        let mut reporters = ReportersConfig::default();
        if ctx.test_options.reporters.junit {
            reporters.junit = RefCell::new(Some(JunitReporter::init()));
        }
        if ctx.test_options.reporters.dots {
            reporters.dots = true;
        }
        if ctx.test_options.reporters.only_failures {
            reporters.only_failures = true;
        } else if Output::is_ai_agent() {
            reporters.only_failures = true; // only-failures defaults to true for ai agents
        }

        // Leaked: `exec()` never returns before process exit, and JS-reentrant
        // test-runner callbacks reach this through `Jest::runner()` /
        // `BunTest::reporter`, so it is shared (`&'static`) from here on.
        let reporter: &'static CommandLineReporter = Box::leak(Box::new(CommandLineReporter {
            jest: TestRunner {
                default_timeout_ms: ctx.test_options.default_timeout_ms,
                concurrent: ctx.test_options.concurrent,
                randomize_seed: if enable_random { Some(seed) } else { None },
                concurrent_test_glob: ctx.test_options.concurrent_test_glob.clone(),
                run_todo: ctx.test_options.run_todo,
                only: Cell::new(ctx.test_options.only),
                bail: ctx.test_options.bail,
                max_concurrency: ctx.test_options.max_concurrency,
                // `test_filter_regex` is an erased `*mut RegularExpression` (see
                // options_types::context); cast back to a typed `NonNull` —
                // kept raw so `matches()` can write through it without
                // laundering shared-ref provenance.
                filter_regex: ctx
                    .test_options
                    .test_filter_regex()
                    .map(|p| p.cast::<jsc::RegularExpression>()),
                snapshots: RefCell::new(Snapshots::init(ctx.test_options.update_snapshots)),
                bun_test_root: bun_test::BunTestRoot::init(),
                // `TestRunner` cannot derive `Default` because of the
                // `&'a TestOptions` field, so spell the remaining fields out
                // explicitly.
                current_file: RefCell::new(jest::CurrentFile::default()),
                files: RefCell::new(jest::FileList::default()),
                index: RefCell::new(jest::FileMap::default()),
                default_timeout_override: Cell::new(u32::MAX),
                test_options: ctx.test_options.clone(),
                unhandled_errors_between_tests: Cell::new(0),
                summary: RefCell::new(Summary::default()),
                node_test_used: Cell::new(false),
            },
            repeat_count: ctx.test_options.repeat_count.max(1),
            last_printed_dot: Cell::new(false),
            worker_ipc_file_idx: Cell::new(None),
            failures_to_repeat_buf: RefCell::new(Vec::new()),
            skips_to_repeat_buf: RefCell::new(Vec::new()),
            todos_to_repeat_buf: RefCell::new(Vec::new()),
            reporters,
            timings: RefCell::new(
                if ctx.test_options.test_worker || ctx.test_options.timings_files.is_empty() {
                    None
                } else {
                    Some(Timings::load(&ctx.test_options.timings_files))
                },
            ),
        }));
        jest::Jest::set_runner(Some(&reporter.jest));

        bun_ast::initialize_store();
        VirtualMachine::init(jsc::virtual_machine::InitOptions {
            // Clone (not take): ParallelRunner::run_as_coordinator → build_worker_argv
            // reads ctx.args.{conditions,define,loaders,tsconfig_override,drop,
            // main_fields,extension_order,env_files,feature_flags,preserve_symlinks,
            // allow_addons,disable_default_env_files,jsx} after this point to forward
            // them to workers.
            transform_options: ctx.args.clone(),
            debugger: core::mem::take(&mut ctx.runtime_options.debugger),
            log: core::ptr::NonNull::new(ctx.log_ptr()),
            env_loader: core::ptr::NonNull::new(&raw mut *env_loader),
            store_fd: ctx.debug.hot_reload != jsc::virtual_machine::HotReload::None,
            smol: ctx.runtime_options.smol,
            is_main_thread: true,
            ..Default::default()
        })?;
        // `init` installed the freshly-boxed VM as this thread's singleton.
        let vm: &mut VirtualMachine = VirtualMachine::get_mut();
        vm.argv = core::mem::take(&mut ctx.passthrough);
        // Clone (not take): build_worker_argv reads ctx.preloads to forward --preload.
        vm.preload = ctx.preloads.clone();
        vm.transpiler.options.rewrite_jest_for_tests = true;
        bun_http::EXPERIMENTAL_HTTP2_CLIENT_FROM_CLI.store(
            ctx.runtime_options.experimental_http2_fetch,
            core::sync::atomic::Ordering::Relaxed,
        );
        bun_http::EXPERIMENTAL_HTTP3_CLIENT_FROM_CLI.store(
            ctx.runtime_options.experimental_http3_fetch,
            core::sync::atomic::Ordering::Relaxed,
        );
        vm.transpiler.options.env.behavior =
            bun_bundler::options::EnvBehavior::LoadAllWithoutInlining;

        let node_env_entry = env_loader.map.get_or_put_without_value(b"NODE_ENV")?;
        if !node_env_entry.found_existing {
            *node_env_entry.key_ptr = Box::<[u8]>::from(&**node_env_entry.key_ptr);
            *node_env_entry.value_ptr = DotEnv::HashTableValue {
                value: Box::<[u8]>::from(b"test" as &[u8]),
            };
        }

        vm.transpiler.configure_defines()?;

        vm.load_extra_env_and_source_code_printer();
        vm.is_main_thread = true;
        VirtualMachine::set_is_main_thread_vm(true);

        if ctx.test_options.isolate {
            vm.test_isolation_enabled = true;
            vm.auto_killer.enabled = true;
        }

        if ctx.test_options.coverage.enabled {
            vm.transpiler.options.code_coverage = true;
            vm.transpiler.options.minify_syntax = false;
            vm.transpiler.options.minify_identifiers = false;
            vm.transpiler.options.minify_whitespace = false;
            vm.transpiler.options.dead_code_elimination = false;
            vm.global().vm().enable_control_flow_profiler();
        }

        // For tests, we default to UTC time zone
        // unless the user inputs TZ="", in which case we use local time zone
        let mut tz_name: &[u8] =
            // We use the string "Etc/UTC" instead of "UTC" so there is no normalization difference.
            b"Etc/UTC";

        if let Some(tz) = vm.transpiler.env().get(b"TZ") {
            tz_name = tz;
        }

        if !tz_name.is_empty() {
            _ = vm
                .global()
                .set_time_zone(&EncodedSlice::from_bytes(tz_name));
        }

        if ctx.test_options.test_worker {
            // Worker mode: skip discovery; files arrive over stdin and
            // results go out over fd 3. Never returns.
            // SAFETY: `vm` is the live per-thread VM; `reporter`/`ctx` outlive
            // this never-returning call.
            ParallelRunner::run_as_worker(reporter, vm, ctx);
        }

        // Start the debugger before we scan for files
        // But, don't block the main thread waiting if they used --inspect-wait.
        vm.ensure_debugger(false)?;

        // Borrowed-slice views (`&[&[u8]]`) over the owned `Vec<Box<[u8]>>`
        // config so the Scanner field types line up; declared before `scanner`
        // so they outlive it.
        let path_ignore_patterns_view: Vec<&[u8]> = ctx
            .test_options
            .path_ignore_patterns
            .iter()
            .map(|b| &**b)
            .collect();
        let filter_names_owned: Vec<&[u8]>;
        #[cfg(windows)]
        let filter_names_normalized_storage: Vec<Box<[u8]>>;
        #[cfg(windows)]
        let filter_names_normalized: Vec<&[u8]>;
        let mut scanner = Scanner::init(&vm.transpiler, ctx.positionals.len()).expect("oom");
        scanner.path_ignore_patterns = &path_ignore_patterns_view[..];
        let has_relative_path = 'hr: {
            for arg in &ctx.positionals {
                if bun_paths::is_absolute(arg)
                    || strings::starts_with(arg, b"./")
                    || strings::starts_with(arg, b"../")
                    || (cfg!(windows)
                        && (strings::starts_with(arg, b".\\")
                            || strings::starts_with(arg, b"..\\")))
                {
                    break 'hr true;
                }
            }
            false
        };
        if has_relative_path {
            // One of the files is a filepath. Instead of treating the
            // arguments as filters, treat them as filepaths
            let file_or_dirnames = &ctx.positionals[1..];
            for arg in file_or_dirnames {
                match scanner.scan(arg) {
                    Ok(()) => {}
                    Err(scanner::ScanError::OutOfMemory) => bun::out_of_memory(),
                    // don't error if multiple are passed; one might fail
                    // but the others may not
                    Err(scanner::ScanError::DoesNotExist) => {
                        if file_or_dirnames.len() == 1 {
                            if Output::is_ai_agent() {
                                pretty_errorln!(
                                    "Test filter <b>{}<r> had no matches in --cwd={}",
                                    bun_fmt::quote(arg),
                                    bun_fmt::quote(FileSystem::instance().top_level_dir)
                                );
                            } else {
                                pretty_errorln!(
                                    "Test filter <b>{}<r> had no matches",
                                    bun_fmt::quote(arg)
                                );
                            }
                            vm.exit_handler.exit_code = 1;
                            vm.is_shutting_down = true;
                            vm.run_with_api_lock_mut(|vm| vm.global_exit());
                        }
                    }
                }
            }
        } else {
            // Treat arguments as filters and scan the codebase
            filter_names_owned = if ctx.positionals.is_empty() {
                Vec::new()
            } else {
                ctx.positionals[1..].iter().map(|b| &**b).collect()
            };

            // Both platforms use a `Vec<&[u8]>` view (already built above as
            // `filter_names_owned`); the Windows branch additionally needs an
            // owned backing `Vec<Box<[u8]>>` for the `/`→`\`-rewritten bytes
            // plus a second view vec over those boxes.
            #[cfg(windows)]
            {
                filter_names_normalized_storage = filter_names_owned
                    .iter()
                    .map(|in_| {
                        let mut to_normalize = in_.to_vec();
                        bun_path::resolve_path::posix_to_platform_in_place::<u8>(&mut to_normalize);
                        to_normalize.into_boxed_slice()
                    })
                    .collect();
                filter_names_normalized = filter_names_normalized_storage
                    .iter()
                    .map(|b| &**b)
                    .collect();
                scanner.filter_names = &filter_names_normalized[..];
            }
            #[cfg(not(windows))]
            {
                scanner.filter_names = &filter_names_owned[..];
            }

            // Own the joined path in a hoisted buffer and borrow from it.
            let dir_to_scan_owned: Vec<u8>;
            let dir_to_scan: &[u8] = 'brk: {
                if !ctx.debug.test_directory.is_empty() {
                    dir_to_scan_owned = resolve_path::join_abs::<bun_path::platform::Auto>(
                        scanner.top_level_dir(),
                        &ctx.debug.test_directory,
                    )
                    .into();
                    break 'brk &dir_to_scan_owned;
                }

                break 'brk scanner.top_level_dir();
            };

            match scanner.scan(dir_to_scan) {
                Ok(()) => {}
                Err(scanner::ScanError::OutOfMemory) => bun::out_of_memory(),
                Err(scanner::ScanError::DoesNotExist) => {
                    if Output::is_ai_agent() {
                        pretty_errorln!(
                            "<red>Failed to scan non-existent root directory for tests:<r> {} in --cwd={}",
                            bun_fmt::quote(dir_to_scan),
                            bun_fmt::quote(FileSystem::instance().top_level_dir)
                        );
                    } else {
                        pretty_errorln!(
                            "<red>Failed to scan non-existent root directory for tests:<r> {}",
                            bun_fmt::quote(dir_to_scan)
                        );
                    }
                    vm.exit_handler.exit_code = 1;
                    vm.is_shutting_down = true;
                    vm.run_with_api_lock_mut(|vm| vm.global_exit());
                }
            }
        }

        let mut all_test_files = scanner.take_found_test_files().expect("oom");
        // Snapshot the count before `test_files` mutably borrows `all_test_files`
        // so the watcher-enable check below can read it without reborrowing.
        let all_test_files_count = all_test_files.len();
        let search_count = scanner.search_count;
        drop(scanner);

        // When --changed or --shard filters the discovered test files
        // down to zero, the "No tests found!" error path is suppressed
        // and the run exits 0 — an empty shard or an unchanged tree
        // is not a misconfiguration.
        let mut pass_with_no_tests_from_filter = false;
        let mut changed_module_graph_files: Vec<Box<[u8]>> = Vec::new();
        // Defer free handled by Drop.
        let mut test_files: &mut [Interned] = if let Some(changed_since) = &ctx.test_options.changed
        {
            'brk: {
                // If the Scanner found nothing, fall through to the existing
                // "no tests found" error path rather than treating it as a
                // --changed success.
                if all_test_files.is_empty() {
                    break 'brk &mut all_test_files[..];
                }
                let result = match ChangedFilesFilter::filter(
                    &ctx,
                    vm,
                    &mut all_test_files[..],
                    changed_since,
                ) {
                    Ok(r) => r,
                    Err(err) => {
                        Output::err(err, "--changed: unable to determine affected tests", ());
                        Global::exit(1);
                    }
                };
                changed_module_graph_files = result.module_graph_files;
                if result.test_files.is_empty() && result.changed_count == 0 {
                    pretty_error!("<r><d>--changed:<r> no changed files, nothing to run\n");
                    pass_with_no_tests_from_filter = true;
                } else if result.test_files.is_empty() {
                    pretty_error!(
                        "<r><d>--changed:<r> {} changed file{}, but no test files are affected\n",
                        result.changed_count,
                        if result.changed_count == 1 { "" } else { "s" }
                    );
                    pass_with_no_tests_from_filter = true;
                } else {
                    pretty_error!(
                        "<r><d>--changed:<r> {} changed file{}, running {}/{} test file{}\n",
                        result.changed_count,
                        if result.changed_count == 1 { "" } else { "s" },
                        result.test_files.len(),
                        result.total_tests,
                        if result.total_tests == 1 { "" } else { "s" }
                    );
                }
                Output::flush();
                break 'brk result.test_files;
            }
        } else {
            &mut all_test_files[..]
        };
        // --shard=M/N: sort the test files for determinism, then keep only
        // every Nth file starting at M-1. This round-robin distribution
        // keeps shards roughly balanced regardless of how many files there
        // are, and is stable across runs and machines as long as the set of
        // test files is the same.
        //
        // Only runs when there are files to shard — if the scanner or
        // --changed already produced an empty list, fall through to the
        // existing "No tests found!" / --changed messaging rather than
        // printing a confusing "running 0/0 test files".
        if let Some(shard) = &ctx.test_options.shard {
            if !test_files.is_empty() {
                let mut write: usize = 0;
                if let Some(timings) = reporter.timings.borrow().as_ref().filter(|t| !t.is_empty())
                {
                    write = timings.select_shard(test_files, *shard);
                } else {
                    index_sort::sort_slice_by(test_files, |a, b| {
                        strings::order(a.as_bytes(), b.as_bytes())
                    });
                    let total = test_files.len();
                    for i in 0..total {
                        if i % (shard.count as usize) == (shard.index as usize) - 1 {
                            test_files[write] = test_files[i];
                            write += 1;
                        }
                    }
                }

                pretty_error!(
                    "<r><d>--shard={}/{}:<r> running {}/{} test file{}\n",
                    shard.index,
                    shard.count,
                    write,
                    test_files.len(),
                    if test_files.len() == 1 { "" } else { "s" }
                );
                Output::flush();

                if write == 0 {
                    // There were test files, but fewer than the shard count so
                    // this shard got none. That's fine — not a "no tests
                    // found" error.
                    pass_with_no_tests_from_filter = true;
                }
                test_files = &mut test_files[0..write];
            }
        }

        // Normally the watcher is only enabled when there are test files to
        // run; `bun test --watch` with nothing matching should still exit.
        // With --changed we always want to keep watching as long as any test
        // files exist, since "nothing changed yet" is the common starting
        // state and editing a source file should kick off a run.
        if !test_files.is_empty()
            || (ctx.test_options.changed.is_some() && all_test_files_count != 0)
        {
            vm.hot_reload = ctx.debug.hot_reload;

            // Install the --changed trigger collector BEFORE the watcher
            // thread starts so a file edit during runAllTests is still
            // recorded. The addFileByPathSlow seeding stays after
            // runAllTests (separate concern; see O_EVTONLY comment
            // below).
            if ctx.test_options.changed.is_some()
                && vm.hot_reload == jsc::virtual_machine::HotReload::Watch
            {
                ChangedFilesFilter::init_watch_trigger();
            }

            match vm.hot_reload {
                jsc::virtual_machine::HotReload::Hot => vm.enable_hot_module_reloading(false, None),
                jsc::virtual_machine::HotReload::Watch => {
                    vm.enable_hot_module_reloading(true, None)
                }
                _ => {}
            }
        }

        let mut coverage_options: CodeCoverageOptions = ctx.test_options.coverage.clone();
        let mut ran_parallel = false;

        if !test_files.is_empty() {
            // Randomize the order of test files if --randomize flag is set
            if let Some(mut rand) = random_instance {
                // `std.Random.shuffle` → Fisher–Yates over `DefaultPrng::next_u64`.
                let n = test_files.len();
                if n > 1 {
                    let mut i = n - 1;
                    while i > 0 {
                        // Unbiased range via 128-bit mul (Lemire).
                        let j = ((rand.next_u64() as u128 * (i as u128 + 1)) >> 64) as usize;
                        test_files.swap(i, j);
                        i -= 1;
                    }
                }
            }

            if ctx.test_options.parallel > 0 {
                ran_parallel = ParallelRunner::run_as_coordinator(
                    reporter,
                    vm,
                    test_files,
                    &mut *ctx,
                    &mut coverage_options,
                )?;
            } else {
                Self::run_all_tests(reporter, vm, test_files);
            }
        }

        // With --changed, only a subset of test files (possibly none) runs,
        // so the module loader won't naturally add every source file to the
        // watcher. Seed it from the module graph so editing any local source
        // file — including files only reachable from tests that were
        // filtered out — still triggers a restart under --watch.
        //
        // This must happen AFTER runAllTests: during the run the module
        // loader registers loaded files with a readable fd, which
        // RuntimeTranspilerStore reuses on the next load. On macOS
        // addFileByPathSlow opens with O_EVTONLY (not readable); seeding
        // first would hand that fd to the transpiler. Seeding after means
        // loaded files are already present (indexOf early-returns) and only
        // the never-loaded filtered-out subgraph gets an O_EVTONLY entry,
        // which the transpiler never touches. The test harness syncs on the
        // "Ran N tests" summary (printed after this), so seeding completes
        // before the next file edit.
        if ctx.test_options.changed.is_some() && vm.is_watcher_enabled() {
            for path in &changed_module_graph_files {
                let _ = vm.watcher_add_file_by_path_slow(path);
            }
        }

        let write_snapshots_success = jest::Jest::runner()
            .unwrap()
            .snapshots
            .borrow_mut()
            .write_inline_snapshots()?;
        jest::Jest::runner()
            .unwrap()
            .snapshots
            .borrow_mut()
            .write_snapshot_file()?;
        {
            let summary: Summary = *reporter.summary();
            if summary.pass > 20
                && !Output::is_ai_agent()
                && !reporter.reporters.dots
                && !reporter.reporters.only_failures
            {
                if summary.skip > 0 {
                    pretty_error!("\n<r><d>{} tests skipped:<r>\n", summary.skip);
                    Output::flush();

                    let error_writer = Output::error_writer();
                    let _ = error_writer.write_all(&reporter.skips_to_repeat_buf.borrow());
                }

                if summary.todo > 0 {
                    if summary.skip > 0 {
                        pretty_error!("\n");
                    }

                    pretty_error!("\n<r><d>{} tests todo:<r>\n", summary.todo);
                    Output::flush();

                    let error_writer = Output::error_writer();
                    let _ = error_writer.write_all(&reporter.todos_to_repeat_buf.borrow());
                }

                if summary.fail > 0 {
                    if summary.skip > 0 || summary.todo > 0 {
                        pretty_error!("\n");
                    }

                    pretty_error!("\n<r><d>{} tests failed:<r>\n", summary.fail);
                    Output::flush();

                    let error_writer = Output::error_writer();
                    let _ = error_writer.write_all(&reporter.failures_to_repeat_buf.borrow());
                }
            }
        }

        Output::flush();

        let mut failed_to_find_any_tests = false;

        if test_files.is_empty() && !pass_with_no_tests_from_filter {
            failed_to_find_any_tests = true;

            // "bun test" - positionals[0] == "test"
            // Therefore positionals starts at [1].
            if ctx.positionals.len() < 2 {
                if Output::is_ai_agent() {
                    // Be very clear to ai.
                    Output::err_generic(
                        "0 test files matching **{{.test,.spec,_test_,_spec_}}.{{js,ts,jsx,tsx}} in --cwd={}",
                        (bun_fmt::quote(FileSystem::instance().top_level_dir),),
                    );
                } else {
                    // Be friendlier to humans.
                    pretty_errorln!(
                        "<yellow>No tests found!<r>\n\nTests need \".test\", \"_test_\", \".spec\" or \"_spec_\" in the filename <d>(ex: \"MyApp.test.ts\")<r>\n"
                    );
                }
            } else {
                if Output::is_ai_agent() {
                    pretty_errorln!(
                        "<yellow>The following filters did not match any test files in --cwd={}:<r>",
                        bun_fmt::quote(FileSystem::instance().top_level_dir)
                    );
                } else {
                    pretty_errorln!(
                        "<yellow>The following filters did not match any test files:<r>"
                    );
                }
                let mut has_file_like: Option<usize> = None;
                for (i, filter) in ctx.positionals[1..]
                    .iter()
                    .enumerate()
                    .map(|(i, f)| (i + 1, f))
                {
                    pretty_error!(" {}", bstr::BStr::new(filter));

                    if has_file_like.is_none()
                        && (strings::ends_with(filter, b".ts")
                            || strings::ends_with(filter, b".tsx")
                            || strings::ends_with(filter, b".js")
                            || strings::ends_with(filter, b".jsx"))
                    {
                        has_file_like = Some(i);
                    }
                }
                if search_count > 0 {
                    pretty_error!("\n{} files were searched ", search_count);
                    Output::print_start_end(ctx.start_time, bun::time::nano_timestamp());
                }

                pretty_errorln!(
                    "\n\n<blue>note<r><d>:<r> Tests need \".test\", \"_test_\", \".spec\" or \"_spec_\" in the filename <d>(ex: \"MyApp.test.ts\")<r>"
                );

                // print a helpful note
                if let Some(i) = has_file_like {
                    pretty_errorln!(
                        "<blue>note<r><d>:<r> To treat the \"{}\" filter as a path, run \"bun test ./{}\"<r>",
                        bstr::BStr::new(&ctx.positionals[i]),
                        bstr::BStr::new(&ctx.positionals[i]),
                    );
                }
            }
            if !Output::is_ai_agent() {
                pretty_error!(
                    "\nLearn more about bun test: <magenta>https://bun.com/docs/cli/test<r>",
                );
            }
        } else {
            pretty_error!("\n");

            if coverage_options.enabled && !ran_parallel {
                let (text, lcov) = (
                    coverage_options.reporters.text,
                    coverage_options.reporters.lcov,
                );
                reporter.generate_code_coverage(
                    vm,
                    &mut coverage_options,
                    text,
                    lcov,
                    Output::enable_ansi_colors_stderr(),
                )?;
            }

            let summary: Summary = *reporter.summary();
            let unhandled_errors_between_tests = reporter.jest.unhandled_errors_between_tests.get();
            let did_label_filter_out_all_tests =
                summary.did_label_filter_out_all_tests() && unhandled_errors_between_tests == 0;

            if !did_label_filter_out_all_tests {
                struct DotIndenter {
                    indent: bool,
                }

                impl core::fmt::Display for DotIndenter {
                    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                        if self.indent {
                            f.write_str(" ")?;
                        }
                        Ok(())
                    }
                }

                let indenter = DotIndenter {
                    indent: !ctx.test_options.reporters.dots,
                };
                if !indenter.indent {
                    pretty_error!("\n");
                }

                // Display the random seed if tests were randomized
                if random_instance.is_some() {
                    pretty_error!("{}<r>--seed={}<r>\n", &indenter, seed);
                }

                if summary.pass > 0 {
                    pretty_error!("<r><green>");
                }

                pretty_error!("{}{:5>} pass<r>\n", &indenter, summary.pass);

                if summary.skip > 0 {
                    pretty_error!("{}<r><yellow>{:5>} skip<r>\n", &indenter, summary.skip);
                } else if summary.skipped_because_label > 0 {
                    pretty_error!(
                        "{}<r><d>{:5>} filtered out<r>\n",
                        &indenter,
                        summary.skipped_because_label
                    );
                }

                if summary.todo > 0 {
                    pretty_error!("{}<r><magenta>{:5>} todo<r>\n", &indenter, summary.todo);
                }

                if summary.fail > 0 {
                    pretty_error!("<r><red>");
                } else {
                    pretty_error!("<r><d>");
                }

                pretty_error!("{}{:5>} fail<r>\n", &indenter, summary.fail);
                if unhandled_errors_between_tests > 0 {
                    pretty_error!(
                        "{}<r><red>{:5>} error{}<r>\n",
                        &indenter,
                        unhandled_errors_between_tests,
                        if unhandled_errors_between_tests > 1 {
                            "s"
                        } else {
                            ""
                        }
                    );
                }

                let mut print_expect_calls = summary.expectations > 0;
                let snapshots = reporter.jest.snapshots.borrow();
                if snapshots.total > 0 {
                    let passed = snapshots.passed;
                    let failed = snapshots.failed;
                    let added = snapshots.added;

                    let mut first = true;
                    if print_expect_calls && added == 0 && failed == 0 {
                        print_expect_calls = false;
                        pretty_error!(
                            "{}{:5>} snapshots, {:5>} expect() calls",
                            &indenter,
                            snapshots.total,
                            summary.expectations
                        );
                    } else {
                        pretty_error!("<d>snapshots:<r> ");

                        if passed > 0 {
                            pretty_error!("<d>{} passed<r>", passed);
                            first = false;
                        }

                        if added > 0 {
                            if first {
                                first = false;
                                pretty_error!("<b>+{} added<r>", added);
                            } else {
                                pretty_error!("<b>, {} added<r>", added);
                            }
                        }

                        if failed > 0 {
                            if first {
                                pretty_error!("<red>{} failed<r>", failed);
                            } else {
                                pretty_error!(", <red>{} failed<r>", failed);
                            }
                        }
                    }

                    pretty_error!("\n");
                }
                drop(snapshots);

                if print_expect_calls {
                    pretty_error!("{}{:5>} expect() calls\n", &indenter, summary.expectations);
                }

                reporter.print_summary();
            } else {
                pretty_error!(
                    "<red>error<r><d>:<r> regex <b>{}<r> matched 0 tests. Searched {} file{} (skipping {} test{}) ",
                    bun_fmt::quote(ctx.test_options.test_filter_pattern.as_ref().unwrap()),
                    summary.files,
                    if summary.files == 1 { "" } else { "s" },
                    summary.skipped_because_label,
                    if summary.skipped_because_label == 1 {
                        ""
                    } else {
                        "s"
                    },
                );
                Output::print_start_end(ctx.start_time, bun::time::nano_timestamp());
            }
        }

        pretty_error!("\n");
        Output::flush();

        reporter.write_junit_report_if_needed();
        if !test_files.is_empty() || ctx.test_options.shard.is_some() {
            reporter.write_timings_if_needed();
        }

        if vm.hot_reload == jsc::virtual_machine::HotReload::Watch {
            vm.run_with_api_lock_mut(Self::run_event_loop_for_watch);
        }
        let summary: Summary = *reporter.summary();

        let should_fail_on_no_tests = !ctx.test_options.pass_with_no_tests
            && (failed_to_find_any_tests || summary.did_label_filter_out_all_tests());
        if should_fail_on_no_tests
            || summary.fail > 0
            || (coverage_options.enabled
                && coverage_options.fractions.failing
                && coverage_options.fail_on_low_coverage)
            || !write_snapshots_success
            || reporter.jest.unhandled_errors_between_tests.get() > 0
        {
            vm.exit_handler.exit_code = 1;
        }
        vm.exit_handler.skip_exit_listeners = skip_exit_listeners(reporter);
        // Must precede the GC-root release below: exit listeners are user JS and may touch still-live state.
        vm.run_with_api_lock_mut(|vm| vm.on_exit());
        // on_exit() already set is_shutting_down; global_exit() asserts it.
        // Release `bun:test` GC roots before `global_exit()` so
        // `Zig__GlobalObject__destructOnExit()`'s `collectNow()` can reach the closures they pin
        // (preload hooks, per-file describe/test callbacks). Clear the runner
        // registration so finalizers running inside the GC can't observe a
        // torn-down `TestRunner`.
        reporter.jest.bun_test_root.deinit_for_exit();
        jest::Jest::set_runner(None);
        vm.run_with_api_lock_mut(|vm| vm.global_exit());
        Ok(())
    }

    fn run_event_loop_for_watch(vm: &mut VirtualMachine) {
        vm.event_loop_ref().tick_possibly_forever();

        loop {
            while vm.is_event_loop_alive() {
                vm.tick();
                vm.event_loop_ref().auto_tick_active();
            }

            vm.event_loop_ref().tick_possibly_forever();
        }
    }

    pub(crate) fn run_all_tests(
        reporter_: &'static CommandLineReporter,
        vm_: &mut VirtualMachine,
        files_: &[Interned],
    ) {
        struct Context<'a> {
            reporter: &'static CommandLineReporter,
            vm: &'a mut VirtualMachine,
            files: &'a [Interned],
        }
        impl<'a> Context<'a> {
            fn begin(&mut self) {
                let reporter = self.reporter;
                let vm = &mut *self.vm;
                let files = self.files;
                debug_assert!(!files.is_empty());

                let isolate = vm.test_isolation_enabled;

                if files.len() > 1 {
                    for (i, file_name) in files[0..files.len() - 1].iter().enumerate() {
                        let started = bun::time::milli_timestamp();
                        if let Err(err) = TestCommand::run(
                            reporter,
                            vm,
                            file_name.as_bytes(),
                            bun_test::FirstLast {
                                first: isolate || i == 0,
                                last: isolate,
                            },
                        ) {
                            handle_top_level_test_error_before_javascript_start(&err);
                        }
                        if let Some(t) = reporter.timings.borrow_mut().as_mut() {
                            t.record_since(file_name.as_bytes(), started);
                        }
                        reporter.jest.default_timeout_override.set(u32::MAX);
                        Global::mimalloc_cleanup(false);
                        if isolate {
                            crate::jsc_hooks::stop_active_handles_for_test_isolation(vm);
                            vm.swap_global_for_test_isolation();
                            reporter
                                .jest
                                .bun_test_root
                                .reset_hook_scope_for_test_isolation();
                        }
                    }
                }

                let last = files[files.len() - 1];
                let started = bun::time::milli_timestamp();
                if let Err(err) = TestCommand::run(
                    reporter,
                    vm,
                    last.as_bytes(),
                    bun_test::FirstLast {
                        first: isolate || files.len() == 1,
                        last: true,
                    },
                ) {
                    handle_top_level_test_error_before_javascript_start(&err);
                }
                if let Some(t) = reporter.timings.borrow_mut().as_mut() {
                    t.record_since(last.as_bytes(), started);
                }
            }
        }

        // No MimallocArena is wired through `vm.arena` on this serial run
        // path; the parallel worker path in runner.rs does wire one.
        // Reintroduce here if it shows up in profiles.
        vm_.event_loop_ref().ensure_waker();
        vm_.run_with_api_lock_mut(|vm| {
            Context {
                reporter: reporter_,
                vm,
                files: files_,
            }
            .begin()
        });
    }

    pub(crate) fn run(
        reporter: &'static CommandLineReporter,
        vm: &mut VirtualMachine,
        file_name: &[u8],
        first_last: bun_test::FirstLast,
    ) -> crate::Result<()> {
        let result = Self::run_inner(reporter, vm, file_name, first_last);

        bun_ast::Expr::data_store_reset();
        bun_ast::Stmt::data_store_reset();

        if let Some(log) = vm.log_mut() {
            if log.errors > 0 {
                let _ = log.print(std::ptr::from_mut::<bun_core::io::Writer>(
                    Output::error_writer(),
                ));
                log.msgs.clear();
                log.errors = 0;
            }
        }

        Output::flush();
        result
    }

    fn run_inner(
        reporter: &'static CommandLineReporter,
        vm: &mut VirtualMachine,
        file_name: &[u8],
        first_last: bun_test::FirstLast,
    ) -> crate::Result<()> {
        // Restore test.only state after each module.
        let prev_only = reporter.jest.only.get();
        scopeguard::defer! { reporter.jest.only.set(prev_only); }

        let resolution = vm.transpiler.resolve_entry_point(file_name)?;
        vm.clear_entry_point()?;

        // `append_slice` interns into the process-static `FilenameStore` and
        // returns `&'static [u8]`.
        let file_path: &'static [u8] = FileSystem::instance()
            .filename_store
            .append_slice(resolution.path_pair.primary.text)
            .expect("oom");
        let file_title = resolve_path::relative(FileSystem::instance().top_level_dir, file_path);
        let file_id = jest::Jest::runner()
            .unwrap()
            .get_or_put_file(file_path)
            .file_id;

        // In Github Actions, append a special prefix that will group
        // subsequent log lines into a collapsable group.
        // https://docs.github.com/en/actions/using-workflows/workflow-commands-for-github-actions#grouping-log-lines
        let file_prefix: &[u8] = if Output::is_github_action() {
            b"::group::"
        } else {
            b""
        };

        let repeat_count = reporter.repeat_count;
        let mut repeat_index: u32 = 0;
        vm.on_unhandled_rejection_ctx = None;
        vm.on_unhandled_rejection = jest::on_unhandled_rejection::on_unhandled_rejection;

        while repeat_index < repeat_count {
            // Clear the module cache before re-running (except for the first run)
            if repeat_index > 0 {
                vm.clear_entry_point()?;
                let entry = EncodedSlice::from_bytes(file_path);
                vm.global().delete_module_registry_entry(&entry)?;
                // Reset per-test snapshot counters so rerun N matches the same
                // snapshot keys as run 1 instead of looking for "test name 2", etc.
                reporter.jest.snapshots.borrow_mut().reset_counts();
            }

            let bun_test_root = &reporter.jest.bun_test_root;
            // Determine if this file should run tests concurrently based on glob pattern
            let should_run_concurrent = reporter.jest.should_file_run_concurrently(file_id);
            bun_test_root.enter_file(file_id, reporter, should_run_concurrent, first_last);
            scopeguard::defer! { bun_test_root.exit_file(); }

            reporter.jest.current_file.borrow_mut().set(
                file_title,
                file_prefix,
                repeat_count,
                repeat_index,
                reporter,
            );

            bun_output::scoped_log!(
                bun_test,
                "loadEntryPointForTestRunner(\"{}\")",
                bstr::BStr::new(file_path)
            );
            // Bun.jsc.Jest.bun_test.debug.group.log → local declare_scope!(bun_test).

            if let Some(junit) = reporter.reporters.junit.borrow_mut().as_mut() {
                junit.file_start_ns = bun::Timespec::now(bun::TimespecMockMode::ForceRealTime).ns();
            }
            // need to wake up so autoTick() doesn't wait for 16-100ms after loading the entrypoint
            vm.wakeup();
            let promise = vm.load_entry_point_for_test_runner(file_path)?;
            // Only count the file once, not once per repeat
            if repeat_index == 0 {
                reporter.summary().files += 1;
            }

            // S012: `JSInternalPromise` is an `opaque_ffi!` ZST — safe `*mut → &mut` deref.
            match jsc::JSInternalPromise::opaque_mut(promise).status() {
                jsc::js_promise::Status::Rejected => {
                    // `vm.global()` returns `&'static`, decoupled from `vm`'s borrow so
                    // `unhandled_rejection(&mut self, ...)` can reborrow.
                    let global = vm.global();
                    let p = jsc::JSInternalPromise::opaque_mut(promise);
                    let (result, promise_js) = (p.result(global.vm()), p.to_js());
                    vm.unhandled_rejection(global, result, promise_js);
                    reporter.summary().fail += 1;

                    let fail_now = reporter.summary().fail;
                    if reporter.jest.bail == fail_now {
                        reporter.print_summary();
                        pretty_error!(
                            "\nBailed out after {} failure{}<r>\n",
                            reporter.jest.bail,
                            if reporter.jest.bail == 1 { "" } else { "s" }
                        );
                        reporter.write_junit_report_if_needed();
                        reporter.write_timings_if_needed();

                        vm.exit_handler.exit_code = 1;
                        vm.is_shutting_down = true;
                        // `global_exit()` diverges, so the `exit_file()` defer
                        // above never fires. Release the active file's
                        // `Strong`s and the preload-hook scope here so
                        // `Zig__GlobalObject__destructOnExit()`'s `collectNow()` can reclaim them,
                        // then clear the runner registration so finalizers
                        // can't observe a partially-torn-down `TestRunner`.
                        bun_test_root.deinit_for_exit();
                        jest::Jest::set_runner(None);
                        vm.run_with_api_lock_mut(|vm| vm.global_exit());
                    }

                    return Ok(());
                }
                _ => {}
            }

            vm.event_loop_ref().tick();

            'blk: {
                // Check if bun_test is available and has tests to run
                let Some(buntest_strong) = bun_test_root.clone_active_file() else {
                    debug_assert!(false);
                    break 'blk;
                };
                let buntest: &bun_test::BunTest = &buntest_strong;

                // Automatically execute bun_test tests
                if buntest.readable_results() == 0 {
                    buntest.add_result(bun_test::ResultMsg::Start);
                }
                buntest.run(vm.global())?;

                // Process event loop while bun_test tests are running
                vm.event_loop_ref().tick();

                let mut prev_unhandled_count = vm.unhandled_error_counter;
                while buntest.phase.get() != bun_test::Phase::Done {
                    if buntest.wants_wakeup.get() {
                        buntest.wants_wakeup.set(false);
                        vm.wakeup();
                    }
                    vm.event_loop_ref().auto_tick();
                    if buntest.phase.get() == bun_test::Phase::Done {
                        break;
                    }
                    vm.event_loop_ref().tick();

                    while prev_unhandled_count < vm.unhandled_error_counter {
                        let _ = vm.global().handle_rejected_promises();
                        prev_unhandled_count = vm.unhandled_error_counter;
                    }
                }

                vm.tick_immediate_tasks();

                // Node parity: a node test file exits only when its loop drains.
                // on_before_exit() drains and dispatches 'beforeExit' like `bun run`;
                // it early-returns when unhandled_error_counter > 0, which is fine
                // here since such a file already failed. Opt-in; one file per process.
                if should_drain_event_loop() {
                    vm.on_before_exit();
                }
                drop(buntest_strong);
            }

            let _ = vm.global().handle_rejected_promises();

            if Output::is_github_action() && reporter.worker_ipc_file_idx.get().is_none() {
                pretty_errorln!("<r>\n::endgroup::\n");
                Output::flush();
            }

            if !vm.test_isolation_enabled {
                // Ensure these never linger across files. Under --isolate this
                // is done by swapGlobalForTestIsolation() (kill+clear) and we
                // need tracking to remain enabled and populated until then.
                vm.auto_killer.clear();
                vm.auto_killer.disable();
            }

            repeat_index += 1;
        }
        if let Some(junit) = reporter.reporters.junit.borrow_mut().as_mut() {
            junit.file_end_ns = bun::Timespec::now(bun::TimespecMockMode::ForceRealTime).ns();
            while !junit.suite_stack.is_empty() {
                let _ = junit.end_test_suite();
            }
            junit.current_file = Box::default();
        }
        Ok(())
    }
}

pub(crate) fn handle_top_level_test_error_before_javascript_start(err: &crate::Error) -> ! {
    if cfg!(debug_assertions) {
        if !matches!(err, crate::Error::ModuleNotFound) {
            bun_core::debug_warn!("Unhandled error: {}", err.name());
        }
    }
    Global::exit(1);
}
