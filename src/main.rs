use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::Print,
    terminal::{
        self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use std::io::{self, Write};
use syntect::{
    easy::HighlightLines,
    highlighting::{Color, Style, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

// ────────────────────────────────────────────────
// データ構造
// ────────────────────────────────────────────────

#[derive(Clone)]
struct Chunk {
    /// ハイライト色付き文字列（改行文字排除済み）
    highlighted: String,
    /// グレー表示用の生文字列（改行文字排除済み）
    plain: String,
    /// 単語境界（スペース直後の最初の実文字）かどうか
    is_word_start: bool,
}

struct Line {
    chunks: Vec<Chunk>,
}

// ────────────────────────────────────────────────
// ユーティリティ
// ────────────────────────────────────────────────

fn utf8_char_len(b: u8) -> usize {
    if b & 0b1111_0000 == 0b1111_0000 { 4 }
    else if b & 0b1110_0000 == 0b1110_0000 { 3 }
    else if b & 0b1100_0000 == 0b1100_0000 { 2 }
    else { 1 }
}

fn is_whitespace_only(s: &str) -> bool {
    s.chars().all(|c| c.is_whitespace())
}

fn style_to_ansi(style: Style, text: &str) -> String {
    let Color { r, g, b, .. } = style.foreground;
    format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, text)
}

fn grey(text: &str) -> String {
    format!("\x1b[38;2;75;75;75m{}\x1b[0m", text)
}

// キャレット（予測文字の上に重なる半透明なブロックを想定）
const CARET: &str = "\x1b[38;2;200;200;200m▌\x1b[0m";

fn skip_first_codepoint(s: &str) -> &str {
    let mut chars = s.chars();
    if chars.next().is_some() {
        chars.as_str()
    } else {
        ""
    }
}

// ────────────────────────────────────────────────
// パース・チャンク構築
// ────────────────────────────────────────────────

fn split_styled_ranges(ranges: &[(Style, &str)]) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for &(style, text) in ranges {
        // Viewport描画ではネイティブの改行を使わないため完全に除去
        let text = text.replace('\r', "").replace('\n', "");
        if text.is_empty() { continue; }

        let bytes = text.as_bytes();
        let len = bytes.len();
        let mut i = 0;
        while i < len {
            let clen = utf8_char_len(bytes[i]);
            if i + clen > len {
                i += 1;
                continue;
            }
            if let Ok(c_str) = std::str::from_utf8(&bytes[i..i + clen]) {
                result.push((style_to_ansi(style, c_str), c_str.to_string()));
                i += clen;
            } else {
                i += 1;
            }
        }
    }
    result
}

fn build_lines(source: &str) -> Result<Vec<Line>> {
    let ss = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();
    let syntax = ss.find_syntax_by_extension("rs").context("Rust syntax not found")?;
    let theme_names = ["base16-ocean.dark", "base16-mocha.dark", "InspiredGitHub"];
    let theme = theme_names
        .iter()
        .find_map(|name| ts.themes.get(*name))
        .unwrap_or_else(|| ts.themes.values().next().expect("no themes"));
    let mut h = HighlightLines::new(syntax, theme);

    let mut lines = Vec::new();

    for line_str in LinesWithEndings::from(source) {
        let ranges = h.highlight_line(line_str, &ss).context("highlight failed")?;
        let raw_chunks = split_styled_ranges(&ranges);

        let mut chunks = Vec::new();
        let mut pending_hi = String::new();
        let mut pending_pl = String::new();
        let mut prev_was_space = true;

        for (hi, pl) in raw_chunks {
            if is_whitespace_only(&pl) {
                pending_hi.push_str(&hi);
                pending_pl.push_str(&pl);
                prev_was_space = true;
            } else {
                chunks.push(Chunk {
                    highlighted: format!("{}{}", pending_hi, hi),
                    plain: format!("{}{}", pending_pl, pl),
                    is_word_start: prev_was_space,
                });
                pending_hi.clear();
                pending_pl.clear();
                prev_was_space = false;
            }
        }

        if !pending_hi.is_empty() {
            if let Some(last) = chunks.last_mut() {
                last.highlighted.push_str(&pending_hi);
                last.plain.push_str(&pending_pl);
            } else {
                // インデントだけの空行
                chunks.push(Chunk {
                    highlighted: pending_hi,
                    plain: pending_pl,
                    is_word_start: false,
                });
            }
        }

        lines.push(Line { chunks });
    }

    Ok(lines)
}

// ────────────────────────────────────────────────
// 状態管理
// ────────────────────────────────────────────────

fn advance_cursor(cline: &mut usize, cchunk: &mut usize, lines: &[Line]) {
    if *cline >= lines.len() { return; }
    *cchunk += 1;
    if *cchunk >= lines[*cline].chunks.len() {
        *cline += 1;
        *cchunk = 0;
        // 入力不要な完全な空行をスキップ
        while *cline < lines.len() && lines[*cline].chunks.is_empty() {
            *cline += 1;
        }
    }
}

// ────────────────────────────────────────────────
// 描画 (Viewport パターン)
// ────────────────────────────────────────────────

fn redraw(
    stdout: &mut impl Write,
    lines: &[Line],
    cursor_line: usize,
    cursor_chunk: usize,
    viewport_top: &mut usize,
) -> Result<()> {
    let (_, rows) = terminal::size()?;
    let height = rows as usize;

    // Viewportの追従ロジック：カーソルが画面外に出たらスクロール
    if cursor_line >= *viewport_top + height {
        *viewport_top = cursor_line - height + 1;
    } else if cursor_line < *viewport_top {
        *viewport_top = cursor_line;
    }

    queue!(stdout, Hide)?;

    // 画面の高さ分だけ、バッファから行を切り出して描画
    for r in 0..height {
        let line_idx = *viewport_top + r;
        queue!(
            stdout,
            MoveTo(0, r as u16),
            Clear(ClearType::CurrentLine)
        )?;

        if line_idx < lines.len() {
            let line = &lines[line_idx];

            if line_idx < cursor_line {
                // 過去行: 全てハイライト色
                for chunk in &line.chunks {
                    queue!(stdout, Print(&chunk.highlighted))?;
                }
            } else if line_idx == cursor_line {
                // 現在行: チャンク単位で状態を判定
                for i in 0..line.chunks.len() {
                    if i < cursor_chunk {
                        queue!(stdout, Print(&line.chunks[i].highlighted))?;
                    } else if i == cursor_chunk {
                        // キャレット描画位置
                        let pl = &line.chunks[i].plain;
                        queue!(stdout, Print(CARET))?;
                        let rest = skip_first_codepoint(pl);
                        if !rest.is_empty() {
                            queue!(stdout, Print(grey(rest)))?;
                        }
                    } else {
                        // 未来チャンク: グレー
                        queue!(stdout, Print(grey(&line.chunks[i].plain)))?;
                    }
                }
            } else {
                // 未来行: 全てグレー
                for chunk in &line.chunks {
                    queue!(stdout, Print(grey(&chunk.plain)))?;
                }
            }
        }
    }

    stdout.flush()?;
    Ok(())
}

// ────────────────────────────────────────────────
// メイン処理
// ────────────────────────────────────────────────

fn run() -> Result<()> {
    // パス修正を適用（.ghq -> ghq）
    let raw_path = "~/ghq/github.com/BurntSushi/ripgrep/crates/core/main.rs";
    let expanded = shellexpand::tilde(raw_path).to_string();
    let source = std::fs::read_to_string(&expanded)
        .with_context(|| format!("Failed to read file: {}", expanded))?;

    let lines = build_lines(&source)?;
    if lines.is_empty() {
        anyhow::bail!("No content to display.");
    }

    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    // DisableLineWrap でターミナルのネイティブな自動改行を無効化（表示崩れ防止）
    execute!(
        stdout,
        EnterAlternateScreen,
        Hide,
        DisableLineWrap,
        Clear(ClearType::All)
    )?;

    let mut cursor_line = 0;
    let mut cursor_chunk = 0;
    let mut viewport_top = 0;

    // 初期化時：最初の空行をスキップ
    while cursor_line < lines.len() && lines[cursor_line].chunks.is_empty() {
        cursor_line += 1;
    }

    redraw(&mut stdout, &lines, cursor_line, cursor_chunk, &mut viewport_top)?;

    loop {
        let ev = event::read()?;

        match ev {
            Event::Key(KeyEvent { code: KeyCode::Esc, .. })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                break;
            }

            Event::Resize(_, _) => {
                // ウィンドウサイズ変更時に適切に再描画
                redraw(&mut stdout, &lines, cursor_line, cursor_chunk, &mut viewport_top)?;
            }

            Event::Key(KeyEvent { code: KeyCode::Right, modifiers, .. })
                if modifiers.contains(KeyModifiers::CONTROL)
                    || modifiers.contains(KeyModifiers::META) =>
            {
                // 単語スキップ（Ctrl+→ / Cmd+→）
                if cursor_line >= lines.len() {
                    cursor_line = 0; cursor_chunk = 0; viewport_top = 0;
                    while cursor_line < lines.len() && lines[cursor_line].chunks.is_empty() {
                        cursor_line += 1;
                    }
                } else {
                    advance_cursor(&mut cursor_line, &mut cursor_chunk, &lines);
                    while cursor_line < lines.len() && !lines[cursor_line].chunks[cursor_chunk].is_word_start {
                        advance_cursor(&mut cursor_line, &mut cursor_chunk, &lines);
                    }
                }
                redraw(&mut stdout, &lines, cursor_line, cursor_chunk, &mut viewport_top)?;
            }

            Event::Key(_) => {
                // 1チャンク進む
                if cursor_line >= lines.len() {
                    cursor_line = 0; cursor_chunk = 0; viewport_top = 0;
                    while cursor_line < lines.len() && lines[cursor_line].chunks.is_empty() {
                        cursor_line += 1;
                    }
                } else {
                    advance_cursor(&mut cursor_line, &mut cursor_chunk, &lines);
                }
                redraw(&mut stdout, &lines, cursor_line, cursor_chunk, &mut viewport_top)?;
            }

            _ => {}
        }
    }

    // 後片付け時に EnableLineWrap を忘れずに戻す
    execute!(stdout, Show, EnableLineWrap, LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        let _ = execute!(io::stdout(), Show, EnableLineWrap, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}