# Security Policy

SageThumbs 2K renders **315 file formats**, most of them parsed from untrusted files, and
its thumbnail/menu code runs **inside Windows Explorer**. We take memory-safety and
crash-isolation seriously — it's 100% pure-Rust, built with `panic = "abort"` and a
`catch_unwind` guard at every COM boundary, and the heavy ImageMagick path runs as a
**sandboxed, time-limited subprocess** (never linked into Explorer). But parsers can still
have bugs, so we want to hear about them.

## Reporting a vulnerability

If you find a security issue — a crash, hang, infinite loop, or memory-safety problem
triggered by a crafted file, or anything that looks exploitable — **please report it
privately** rather than opening a public issue, so it can be fixed before it's disclosed:

- **Preferred:** GitHub's private
  **[Report a vulnerability](https://github.com/LunarWerxs/SageThumbs-2k/security/advisories/new)**
  (the repo's *Security → Advisories* tab), or
- open a normal issue **asking us to contact you** (don't include the exploit details there).

Please include:

- the **file** that triggers it (or a link to it),
- the **SageThumbs 2K version** (Settings → About) and your **Windows build**, and
- what you observed (crash / hang / wrong render / etc.).

We'll acknowledge within a few days and keep you updated through to a fix and release.

## Supported versions

The **latest release** on the
[Releases page](https://github.com/LunarWerxs/SageThumbs-2k/releases) is the supported
version — please update and confirm the issue still reproduces before reporting.

Thank you for helping keep SageThumbs 2K users safe.
