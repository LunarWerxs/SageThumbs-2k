//! The per-input result of a multi-input verb: which inputs a composer left out, and why.
//!
//! 2026-09-05 audit, F31. `combine_to_pdf_paged` and `combine_to_cbz` each tracked their
//! dropped inputs as a bare count, the Explorer verbs turned that into a note, and the CLI
//! and MCP wrappers threw it away: a PDF built from one good PNG and one corrupt PNG exited
//! 0 printing only the output path. One shared structure now carries the output, the count
//! that made it in, and every omitted input with a machine-readable cause, so the CLI text,
//! the MCP JSON and the Explorer report all read the same thing. A later batch grows this
//! into the per-file result model every bulk verb reports through; keep additions here, not
//! in the callers.
//!
//! That batch is F11, and it is [`FileOutcome`] / [`BatchReport`] below: the Convert dialog
//! kept the NAMES of the files that failed and `st2k batch` reduced each file to a bool, so
//! neither surface could tell a corrupt input from a destination it could not write, and a
//! script had nothing to retry from. Both front ends build the same per-file record now.

use std::path::PathBuf;

/// Why one input was left out. A closed set rather than free text, so a script or an agent
/// can branch on it: a file that is missing or locked is retried or fixed differently from
/// one whose bytes are not an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmitCause {
    /// The file could not be read at all: missing, locked, unreadable, or past the size cap.
    Unreadable,
    /// The bytes were read but no decoder accepted them (corrupt, or not an image).
    Undecodable,
    /// Decoded, but re-encoding it for the output format failed.
    Unencodable,
    /// The output could not be claimed or replaced: something else owns that name, the
    /// destination is read-only or locked, or the volume is full. A retry to a different
    /// folder is what fixes this one, which is why it is not folded into `Unencodable`
    /// (2026-09-05 audit, F11).
    Unwritable,
}

impl OmitCause {
    /// The stable token the CLI text and the MCP JSON carry.
    pub fn as_str(self) -> &'static str {
        match self {
            OmitCause::Unreadable => "unreadable",
            OmitCause::Undecodable => "undecodable",
            OmitCause::Unencodable => "unencodable",
            OmitCause::Unwritable => "unwritable",
        }
    }
}

/// One input a composer left out of its output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Omitted {
    /// The input path exactly as the caller passed it.
    pub input: String,
    pub cause: OmitCause,
    /// The underlying error text, for a person reading the report; `cause` is what a
    /// machine reads.
    pub detail: String,
}

impl Omitted {
    pub(crate) fn new(input: &str, cause: OmitCause, detail: impl std::fmt::Display) -> Self {
        Omitted {
            input: input.to_string(),
            cause,
            detail: detail.to_string(),
        }
    }

    /// One tab-separated line, `omitted<TAB>input<TAB>cause<TAB>detail`, the same shape in a
    /// partial success report and in a `--strict` refusal so a script parses one format.
    pub fn as_line(&self) -> String {
        format!(
            "omitted\t{}\t{}\t{}",
            self.input,
            self.cause.as_str(),
            self.detail
        )
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "input": self.input,
            "cause": self.cause.as_str(),
            "detail": self.detail,
        })
    }
}

/// What a composer does when an input has to be left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnOmit {
    /// Build the output from the usable inputs and report the rest: the Explorer verbs'
    /// behaviour, and the CLI/MCP default.
    #[default]
    Report,
    /// Write nothing and fail, naming every input that would have been left out
    /// (`--strict`, or `"strict": true` over MCP).
    Fail,
}

/// What a multi-input composer (PDF, CBZ) produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Combined {
    /// The file that was written.
    pub output: PathBuf,
    /// How many inputs made it into `output`.
    pub used: usize,
    /// The inputs that did not, in the composer's (natural-sorted) order.
    pub omitted: Vec<Omitted>,
}

impl Combined {
    /// How many inputs the caller asked for: the ones used plus the ones omitted.
    pub fn requested(&self) -> usize {
        self.used + self.omitted.len()
    }

    pub fn is_partial(&self) -> bool {
        !self.omitted.is_empty()
    }

    /// The machine-readable form the MCP tools return and `--json` prints. `status` is the
    /// one field a caller has to look at: `"ok"` means every input is in the output.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "output": self.output.display().to_string(),
            "status": if self.is_partial() { "partial" } else { "ok" },
            "requested": self.requested(),
            "combined": self.used,
            "omitted": self.omitted.iter().map(Omitted::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Whether one input got everything the run asked of it. A field rather than "is `detail`
/// empty": a front end that fails a file without a sentence to show for it must still count
/// as a failure, and deriving the verdict from the text would silently make that a success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// Every requested output for this input was written.
    Ok,
    /// At least one was not. `FileOutcome::output` may still hold what did get written.
    Failed,
}

impl FileStatus {
    /// The stable token the CLI lines and the JSON carry.
    pub fn as_str(self) -> &'static str {
        match self {
            FileStatus::Ok => "ok",
            FileStatus::Failed => "failed",
        }
    }
}

/// What one input produced in a BULK run: the shared per-file record the Convert dialog
/// and the CLI/MCP `batch` verb both report through (2026-09-05 audit, F11).
///
/// `cause` is what a machine branches on, `detail` is the sentence a person reads. `cause`
/// is `Option` on purpose: a front end that cannot say WHICH phase failed leaves it unset
/// rather than guessing a bucket, so an absent cause is never mistaken for a measured one.
/// The dialog is that case, and deliberately so, its three converters each return one
/// opaque error and its report shows the sentence, not a token.
///
/// There is no `decoder` field even though the audit offers one: the tier chain only LOGS
/// which decoder took a file (`decode.rs`), and threading a name back out of every tier is
/// a change to the hot path that this report does not justify. `elapsed_ms` is carried,
/// since timing one call needs nothing from the decoder at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutcome {
    /// The input path exactly as the caller passed it, so a failed entry is retryable
    /// verbatim.
    pub input: String,
    pub status: FileStatus,
    /// What was written, when anything was. A file whose first output landed but whose
    /// second requested size did not carries BOTH a path and a failure.
    pub output: Option<PathBuf>,
    /// `None` means this file succeeded.
    pub cause: Option<OmitCause>,
    /// The underlying error text; empty on success.
    pub detail: String,
    /// Wall-clock time this one file took, including its decode.
    pub elapsed_ms: u64,
}

impl FileOutcome {
    /// A file every requested output was written for.
    pub fn ok(input: &str, output: PathBuf) -> Self {
        FileOutcome {
            input: input.to_string(),
            status: FileStatus::Ok,
            output: Some(output),
            cause: None,
            detail: String::new(),
            elapsed_ms: 0,
        }
    }

    /// A file that did not fully convert. `cause` is `None` where the caller cannot tell
    /// which phase failed (see the type doc).
    pub fn failed(input: &str, cause: Option<OmitCause>, detail: impl std::fmt::Display) -> Self {
        FileOutcome {
            input: input.to_string(),
            status: FileStatus::Failed,
            output: None,
            cause,
            detail: detail.to_string(),
            elapsed_ms: 0,
        }
    }

    /// Attach the output a partly-successful file did manage to write.
    #[must_use]
    pub fn produced(mut self, output: Option<PathBuf>) -> Self {
        self.output = output;
        self
    }

    /// Attach how long this file took.
    #[must_use]
    pub fn timed(mut self, elapsed: std::time::Duration) -> Self {
        self.elapsed_ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
        self
    }

    pub fn is_ok(&self) -> bool {
        matches!(self.status, FileStatus::Ok)
    }

    /// One tab-separated line, `failed<TAB>input<TAB>cause<TAB>detail`, deliberately the
    /// same shape as [`Omitted::as_line`] so a script parses one format across every verb
    /// here. `-` stands in for a cause the front end did not measure, never a blank column.
    pub fn as_line(&self) -> String {
        format!(
            "failed\t{}\t{}\t{}",
            self.input,
            self.cause.map_or("-", OmitCause::as_str),
            self.detail
        )
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "input": self.input,
            "output": self.output.as_ref().map(|p| p.display().to_string()),
            "status": self.status.as_str(),
            "cause": self.cause.map(OmitCause::as_str),
            "detail": self.detail,
            "elapsed_ms": self.elapsed_ms,
        })
    }
}

/// Everything one bulk run did, in input order. Partial success is deliberate policy here
/// (one bad file must not cost the other 999), so the report has to carry enough for a
/// caller to act on the difference.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchReport {
    pub files: Vec<FileOutcome>,
    /// Cloud placeholders left alone rather than downloaded. Counted, not listed, matching
    /// what the CLI already said about them.
    pub skipped_offline: usize,
}

impl BatchReport {
    /// How many inputs were attempted.
    pub fn requested(&self) -> usize {
        self.files.len()
    }

    pub fn succeeded(&self) -> usize {
        self.files.iter().filter(|f| f.is_ok()).count()
    }

    /// The failures, in input order: the retry list.
    pub fn failures(&self) -> impl Iterator<Item = &FileOutcome> {
        self.files.iter().filter(|f| !f.is_ok())
    }

    /// `"ok"` when every input converted, `"failed"` when none did, `"partial"` between.
    /// An empty run reports `"ok"`; the callers refuse an empty input list before they get
    /// this far, so there is no run to call failed.
    pub fn status(&self) -> &'static str {
        match self.succeeded() {
            n if n == self.requested() => "ok",
            0 => "failed",
            _ => "partial",
        }
    }

    /// The machine-readable form `--json` prints and the MCP tool returns.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.status(),
            "requested": self.requested(),
            "succeeded": self.succeeded(),
            "failed": self.requested() - self.succeeded(),
            "skipped_offline": self.skipped_offline,
            "results": self.files.iter().map(FileOutcome::to_json).collect::<Vec<_>>(),
        })
    }

    /// One [`FileOutcome::as_line`] per failure, newline-separated and empty when there
    /// were none, for appending to a human summary.
    pub fn failure_lines(&self) -> String {
        self.failures()
            .map(|f| format!("\n{}", f.as_line()))
            .collect()
    }

    /// The retry list read back out of the JSON [`Self::to_json`] wrote: every `results`
    /// entry whose `status` is not `"ok"`, by the `input` it was given, in report order.
    /// This is what `st2k batch --retry-from <report.json>` runs (2026-09-05 audit, E01).
    ///
    /// Beside the writer on purpose, so the two cannot drift: a renamed key fails the
    /// round-trip test below before it fails a user. Anything that is not this report's
    /// shape is refused with the reason, rather than read as an empty retry list: `pdf
    /// --json` has `status` but no `results`, `batch info` is a bare array, and a file
    /// that is not JSON at all is most likely an image handed to the wrong flag. An entry
    /// with an unknown status is retried, not skipped, since only `"ok"` means done.
    pub fn failed_inputs_from_json(text: &str) -> Result<Vec<String>, String> {
        let doc: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        let Some(results) = doc.get("results").and_then(serde_json::Value::as_array) else {
            return Err("not a batch report (no \"results\" array)".to_string());
        };
        if !doc.get("status").is_some_and(serde_json::Value::is_string) {
            return Err("not a batch report (no \"status\")".to_string());
        }
        let mut inputs = Vec::new();
        for (i, entry) in results.iter().enumerate() {
            let input = entry.get("input").and_then(serde_json::Value::as_str);
            let status = entry.get("status").and_then(serde_json::Value::as_str);
            let (Some(input), Some(status)) = (input, status) else {
                return Err(format!(
                    "not a batch report (results[{i}] has no \"input\" and \"status\")"
                ));
            };
            if status != FileStatus::Ok.as_str() {
                inputs.push(input.to_string());
            }
        }
        Ok(inputs)
    }
}

/// Split the composers' per-input attempts into the pages that worked and the inputs that
/// did not, keeping each list in input order.
pub(crate) fn partition<T>(attempts: Vec<Result<T, Omitted>>) -> (Vec<T>, Vec<Omitted>) {
    let mut ok = Vec::with_capacity(attempts.len());
    let mut omitted = Vec::new();
    for a in attempts {
        match a {
            Ok(v) => ok.push(v),
            Err(o) => omitted.push(o),
        }
    }
    (ok, omitted)
}

/// The text of a refusal: `headline`, then one [`Omitted::as_line`] per left-out input. Used
/// both when nothing at all could be used and when `OnOmit::Fail` declines a partial output.
pub(crate) fn refusal(headline: &str, omitted: &[Omitted]) -> String {
    let mut s = headline.to_string();
    for o in omitted {
        s.push('\n');
        s.push_str(&o.as_line());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JSON is the contract an agent reads: `status` flips on the first omission, the
    /// counts add up, and each omitted entry carries the stable cause token, not a Debug
    /// rendering of the enum.
    #[test]
    fn combined_json_carries_status_counts_and_causes() {
        let all_good = Combined {
            output: PathBuf::from("out.pdf"),
            used: 2,
            omitted: Vec::new(),
        };
        let v = all_good.to_json();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["requested"], 2);
        assert_eq!(v["combined"], 2);
        assert_eq!(v["omitted"].as_array().map(Vec::len), Some(0));

        let partial = Combined {
            output: PathBuf::from("out.pdf"),
            used: 1,
            omitted: vec![
                Omitted::new("bad.png", OmitCause::Undecodable, "not a png"),
                Omitted::new("gone.png", OmitCause::Unreadable, "os error 2"),
            ],
        };
        let v = partial.to_json();
        assert_eq!(v["status"], "partial");
        assert_eq!(v["requested"], 3);
        assert_eq!(v["combined"], 1);
        assert_eq!(v["omitted"][0]["cause"], "undecodable");
        assert_eq!(v["omitted"][1]["cause"], "unreadable");
        assert_eq!(v["omitted"][1]["input"], "gone.png");
        assert_eq!(
            partial.omitted[0].as_line(),
            "omitted\tbad.png\tundecodable\tnot a png"
        );
    }

    /// The batch JSON is the contract a script reads (2026-09-05 audit, F11): the counts
    /// add up, `status` says whether it needs to look further, and each entry carries the
    /// input path it was given plus a stable cause token. A front end that could not
    /// measure the phase reports no cause at all rather than a guessed one.
    #[test]
    fn batch_report_json_carries_counts_causes_and_the_retry_paths() {
        let report = BatchReport {
            files: vec![
                FileOutcome::ok("a.png", PathBuf::from("a.webp")),
                FileOutcome::failed("b.png", Some(OmitCause::Undecodable), "cannot decode b.png"),
                FileOutcome::failed("c.png", Some(OmitCause::Unwritable), "Access is denied.")
                    .timed(std::time::Duration::from_millis(7)),
                FileOutcome::failed("d.png", None, "something the dialog cannot bucket"),
            ],
            skipped_offline: 2,
        };
        let v = report.to_json();
        assert_eq!(v["status"], "partial");
        assert_eq!(v["requested"], 4);
        assert_eq!(v["succeeded"], 1);
        assert_eq!(v["failed"], 3);
        assert_eq!(v["skipped_offline"], 2);
        assert_eq!(v["results"][0]["status"], "ok");
        assert_eq!(v["results"][0]["output"], "a.webp");
        assert_eq!(v["results"][0]["cause"], serde_json::Value::Null);
        assert_eq!(v["results"][1]["cause"], "undecodable");
        assert_eq!(v["results"][2]["cause"], "unwritable");
        assert_eq!(v["results"][2]["elapsed_ms"], 7);
        assert_eq!(v["results"][3]["cause"], serde_json::Value::Null);
        assert_eq!(
            v["results"][3]["status"], "failed",
            "no cause is not the same thing as no failure"
        );
        assert_eq!(
            report
                .failures()
                .map(|f| f.input.as_str())
                .collect::<Vec<_>>(),
            ["b.png", "c.png", "d.png"],
            "the retry list is every failure, in input order"
        );
        assert_eq!(
            report.failures().next().map(FileOutcome::as_line),
            Some("failed\tb.png\tundecodable\tcannot decode b.png".to_string())
        );
        assert!(
            report.failure_lines().starts_with("\nfailed\tb.png"),
            "the lines append straight onto a summary"
        );

        // The two ends of the scale, which the counts above cannot show.
        let all_good = BatchReport {
            files: vec![FileOutcome::ok("a.png", PathBuf::from("a.webp"))],
            skipped_offline: 0,
        };
        assert_eq!(all_good.status(), "ok");
        assert_eq!(all_good.failure_lines(), "");
        let all_bad = BatchReport {
            files: vec![FileOutcome::failed(
                "a.png",
                Some(OmitCause::Unreadable),
                "gone",
            )],
            skipped_offline: 0,
        };
        assert_eq!(all_bad.status(), "failed");
    }

    /// The retry list survives a trip through the JSON (2026-09-05 audit, E01): what
    /// `failures()` lists is exactly what comes back from the text `to_json` wrote, and a
    /// document of any other shape is refused with a reason rather than read as "nothing
    /// to retry". The refusal cases are the real neighbours: `pdf --json`, `batch info`,
    /// and a file that is not JSON.
    #[test]
    fn the_retry_list_round_trips_through_the_json_and_other_shapes_are_refused() {
        let report = BatchReport {
            files: vec![
                FileOutcome::ok("a.png", PathBuf::from("a.webp")),
                FileOutcome::failed("b.png", Some(OmitCause::Undecodable), "cannot decode"),
                FileOutcome::failed("c.png", None, "no cause measured"),
            ],
            skipped_offline: 1,
        };
        let text = report.to_json().to_string();
        assert_eq!(
            BatchReport::failed_inputs_from_json(&text),
            Ok(vec!["b.png".to_string(), "c.png".to_string()]),
            "the retry list is every failure, in report order, cause or no cause"
        );
        let clean = BatchReport {
            files: vec![FileOutcome::ok("a.png", PathBuf::from("a.webp"))],
            skipped_offline: 0,
        };
        assert_eq!(
            BatchReport::failed_inputs_from_json(&clean.to_json().to_string()),
            Ok(Vec::new()),
            "a clean report has nothing to retry, which is not an error here"
        );

        let combined = Combined {
            output: PathBuf::from("out.pdf"),
            used: 1,
            omitted: vec![Omitted::new("bad.png", OmitCause::Undecodable, "x")],
        };
        for (what, doc) in [
            ("a pdf/cbz report", combined.to_json().to_string()),
            (
                "a batch info array",
                r#"[{"input":"a.png","width":8}]"#.to_string(),
            ),
            (
                "an object with no results",
                r#"{"status":"ok"}"#.to_string(),
            ),
            ("results with no status", r#"{"results":[]}"#.to_string()),
            (
                "a results entry with no input",
                r#"{"status":"partial","results":[{"status":"failed"}]}"#.to_string(),
            ),
            ("not JSON", "\u{89}PNG\r\n".to_string()),
        ] {
            let err = BatchReport::failed_inputs_from_json(&doc)
                .expect_err(&format!("{what} must be refused"));
            assert!(
                err.starts_with("not "),
                "{what}: the refusal must say what the file is not: {err}"
            );
        }
    }

    /// A file whose FIRST output was written and whose second was not is a failure that
    /// still has something to reveal, so the record has to carry both.
    #[test]
    fn a_partly_written_file_keeps_its_output_and_its_failure() {
        let partial = FileOutcome::failed("a.png", None, "the 720p size did not write")
            .produced(Some(PathBuf::from("a_1080.png")));
        assert!(!partial.is_ok());
        assert_eq!(partial.output, Some(PathBuf::from("a_1080.png")));
        assert_eq!(partial.to_json()["output"], "a_1080.png");
        assert_eq!(partial.to_json()["status"], "failed");
    }

    #[test]
    fn partition_keeps_input_order_on_both_sides() {
        let attempts: Vec<Result<u32, Omitted>> = vec![
            Ok(1),
            Err(Omitted::new("a", OmitCause::Unreadable, "x")),
            Ok(3),
            Err(Omitted::new("b", OmitCause::Undecodable, "y")),
        ];
        let (ok, omitted) = partition(attempts);
        assert_eq!(ok, vec![1, 3]);
        assert_eq!(
            omitted.iter().map(|o| o.input.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(
            refusal("nothing usable", &omitted),
            "nothing usable\nomitted\ta\tunreadable\tx\nomitted\tb\tundecodable\ty"
        );
    }
}
