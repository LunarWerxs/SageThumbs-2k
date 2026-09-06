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
}

impl OmitCause {
    /// The stable token the CLI text and the MCP JSON carry.
    pub fn as_str(self) -> &'static str {
        match self {
            OmitCause::Unreadable => "unreadable",
            OmitCause::Undecodable => "undecodable",
            OmitCause::Unencodable => "unencodable",
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
