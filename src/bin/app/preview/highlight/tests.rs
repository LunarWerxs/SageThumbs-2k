use super::{
    col_at, disp_extent, lang_from_name_or_shebang, lang_from_shebang, paint_lines, word_at, Lang,
};

/// Every raw char boundary must round-trip: measure its x with `disp_extent` (the paint
/// side, `GetTextExtentPoint32W`), feed that x back through `col_at` (the hit-test side,
/// `GetTextExtentExPointW`) and land on the same boundary — proving the two GDI measures
/// agree and the tab/surrogate display-unit grouping is right.
#[test]
fn col_at_roundtrips_disp_extent() {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        CreateFontW, DeleteObject, GetDC, ReleaseDC, SelectObject, CLIP_DEFAULT_PRECIS,
        DEFAULT_CHARSET, DEFAULT_QUALITY, OUT_DEFAULT_PRECIS,
    };
    unsafe {
        let hdc = GetDC(None);
        assert!(!hdc.is_invalid());
        let face: Vec<u16> = "Consolas\0".encode_utf16().collect();
        let font = CreateFontW(
            -13,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            DEFAULT_QUALITY,
            Default::default(),
            PCWSTR(face.as_ptr()),
        );
        let old = SelectObject(hdc, font.into());
        let line = "\tlet grüße = vec![1, 42];\t// done 🚀 end";
        assert_eq!(col_at(hdc, line, 0), 0);
        for (i, c) in line.char_indices() {
            let b = i + c.len_utf8();
            let x = disp_extent(hdc, line, b);
            assert_eq!(col_at(hdc, line, x), b, "boundary {b} (after {c:?})");
        }
        SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
        ReleaseDC(None, hdc);
    }
}

/// The whole point of the `BLOCK_CACHE`: a repaint that only shows a line DEEP inside an
/// unterminated block comment must still colour it as a comment, using the cached per-line
/// `in_block` table built on an earlier full pass — not silently default to "not in a
/// comment" for a line it never actually re-lexed. A bug in the cache/seed logic would show
/// up as that line being coloured `fg` (plain text) instead of the theme's comment colour.
#[test]
fn cached_in_block_state_survives_a_visible_range_that_skips_the_comment_open() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{COLORREF, RECT};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreateSolidBrush, DeleteDC,
        DeleteObject, FillRect, GetDC, GetPixel, GetTextMetricsW, ReleaseDC, SelectObject,
        SetBkMode, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, OUT_DEFAULT_PRECIS,
        TEXTMETRICW, TRANSPARENT,
    };

    // Squared RGB distance between two 0x00BBGGRR COLORREFs — good enough to tell "closer to
    // A than to B" apart without caring about exact anti-aliased blending.
    fn dist2(a: u32, b: u32) -> i64 {
        let ch = |c: u32, shift: u32| ((c >> shift) & 0xFF) as i64;
        (0..3)
            .map(|i| {
                let s = i * 8;
                (ch(a, s) - ch(b, s)).pow(2)
            })
            .sum()
    }

    unsafe {
        let screen = GetDC(None);
        let memdc = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, 2000, 2000);
        let old_bmp = SelectObject(memdc, bmp.into());
        ReleaseDC(None, screen);

        let face: Vec<u16> = "Consolas\0".encode_utf16().collect();
        let font = CreateFontW(
            -20,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            DEFAULT_QUALITY,
            Default::default(),
            PCWSTR(face.as_ptr()),
        );

        let bg = CreateSolidBrush(COLORREF(0x0000_0000)); // black canvas
        let full = RECT {
            left: 0,
            top: 0,
            right: 2000,
            bottom: 2000,
        };
        FillRect(memdc, &full, bg);
        let _ = DeleteObject(bg.into());
        // `paint_lines` itself never touches bk mode (its real caller sets this once for the
        // whole pane) — without it, GDI's default OPAQUE mode paints every glyph cell's
        // background in the DC's default bk colour (white) regardless of the glyph's own
        // foreground colour, which would swamp the very distinction this test is checking.
        SetBkMode(memdc, TRANSPARENT);

        let old_font = SelectObject(memdc, font.into());
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(memdc, &mut tm);
        let line_h = tm.tmHeight + tm.tmExternalLeading;
        SelectObject(memdc, old_font);

        // Line 0: ordinary code. Line 1: opens a block comment that never closes anywhere
        // in this text. Lines 2..30: plain letters (no keywords/strings/numbers) — content
        // that tokenizes as `Tag::Plain` (the caller's `fg`) if NOT inside the comment, or
        // one `Tag::Comment` run if it correctly is.
        const DEEP_LINE: usize = 15; // 0-based; well past line 1's comment-open
        let mut text = String::from("fn a() {}\n/* never closes\n");
        for _ in 0..28 {
            text.push_str("abcdefghijklmnop\n");
        }
        let fg = 0x00FF_FFFF; // white — Tag::Plain's colour in this call
        let comment = crate::dark::CODE_COMMENT().0;

        // First call: MISS. clip covers only line 0 (nothing past the comment-open is
        // actually drawn), but a MISS still lexes EVERY line (unconditionally) to build the
        // cache — see `paint_lines`' `should_lex`.
        let _ = paint_lines(
            memdc,
            &text,
            Lang::Rust,
            4,
            0,
            1900,
            0,
            line_h,
            font,
            fg,
            None,
            None,
        );

        // Second call: HIT. clip covers ONLY `DEEP_LINE` — the first call never drew it, so
        // if the cache/seed path is broken this is the ONLY chance to catch a wrong colour.
        let y_top = DEEP_LINE as i32 * line_h;
        let _ = paint_lines(
            memdc,
            &text,
            Lang::Rust,
            4,
            0,
            1900,
            y_top,
            y_top + line_h,
            font,
            fg,
            None,
            None,
        );

        // Sample a strip of pixels in the CODE column (past the line-number gutter — mirrors
        // `paint_lines`' own `code_x` math exactly, so this can't accidentally land on a
        // line-number digit instead of the actual code glyphs) and keep the one most
        // different from the black background — i.e. the strongest "ink" sample — then
        // check it reads as the comment colour, not `fg`.
        let total_lines = text.split('\n').count().max(1);
        let char_w = tm.tmAveCharWidth.max(1);
        let digits = total_lines.to_string().len() as i32;
        let code_x = 4 + digits * char_w + char_w * 2;
        let mut best: Option<(i64, u32)> = None;
        for dx in 0..(char_w * 6) {
            let px = GetPixel(memdc, code_x + dx, y_top + line_h / 2);
            let d = dist2(px.0, 0);
            if best.is_none_or(|(bd, _)| d > bd) {
                best = Some((d, px.0));
            }
        }
        let (_, sampled) = best.expect("sampled at least one pixel");
        assert!(
            dist2(sampled, comment) < dist2(sampled, fg),
            "deep line must render as the CACHED comment state, not plain fg \
             (sampled {sampled:#08X}, comment {comment:#08X}, fg {fg:#08X})"
        );

        SelectObject(memdc, old_bmp);
        let _ = DeleteObject(font.into());
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(memdc);
    }
}

#[test]
fn word_at_selects_identifiers_and_singles() {
    let t = "fn räum_1() {\n\tlet x = 42;\n}";
    let f = t.find("räum_1").unwrap();
    assert_eq!(word_at(t, f), (f, f + "räum_1".len())); // start of word
    assert_eq!(word_at(t, f + 3), (f, f + "räum_1".len())); // mid-word (after the 2-byte 'ä')
    let paren = t.find('(').unwrap();
    assert_eq!(word_at(t, paren), (paren, paren + 1)); // punctuation = itself
    let nl = t.find('\n').unwrap();
    assert_eq!(word_at(t, nl), (nl, nl)); // line break = nothing
    assert_eq!(word_at(t, t.len()), (t.len(), t.len())); // end of doc
    let num = t.find("42").unwrap();
    assert_eq!(word_at(t, num + 1), (num, num + 2)); // digits group like a word
}

#[test]
fn name_fallback_maps_known_files_case_insensitively() {
    assert!(matches!(
        lang_from_name_or_shebang("Makefile", ""),
        Lang::Sh
    ));
    assert!(matches!(
        lang_from_name_or_shebang("DOCKERFILE", ""),
        Lang::Sh
    ));
    assert!(matches!(
        lang_from_name_or_shebang("Jenkinsfile", ""),
        Lang::Java
    ));
    assert!(matches!(
        lang_from_name_or_shebang("Vagrantfile", ""),
        Lang::Ruby
    ));
    assert!(matches!(
        lang_from_name_or_shebang(".GitIgnore", ""),
        Lang::Toml
    ));
    assert!(matches!(lang_from_name_or_shebang("go.mod", ""), Lang::Go));
    assert!(matches!(
        lang_from_name_or_shebang("Cargo.lock", ""),
        Lang::Toml
    ));
    // an unlisted name with no shebang falls all the way through to Plain.
    assert!(matches!(
        lang_from_name_or_shebang("readme", "just text"),
        Lang::Plain
    ));
}

#[test]
fn shebang_fallback_maps_known_interpreters() {
    assert!(matches!(
        lang_from_shebang("#!/bin/bash\necho hi"),
        Lang::Sh
    ));
    assert!(matches!(
        lang_from_shebang("#!/usr/bin/env python3\nprint(1)"),
        Lang::Py
    ));
    assert!(matches!(
        lang_from_shebang("#!/usr/bin/env node\nconsole.log(1)"),
        Lang::Js
    ));
    assert!(matches!(
        lang_from_shebang("#!/usr/bin/ruby\nputs 1"),
        Lang::Ruby
    ));
    assert!(matches!(
        lang_from_shebang("#!/usr/bin/php\n<?php"),
        Lang::Php
    ));
    assert!(matches!(
        lang_from_shebang("#!/usr/bin/perl\nprint 1;"),
        Lang::Perl
    ));
    // versioned interpreter names must still hit by prefix, same as python3/ruby2.7.
    assert!(matches!(
        lang_from_shebang("#!/usr/bin/env perl5\nprint 1;"),
        Lang::Perl
    ));
    // the first line isn't a shebang at all (one appears later, which must not count).
    assert!(matches!(
        lang_from_shebang("just some text\n#!/bin/bash"),
        Lang::Plain
    ));
}
