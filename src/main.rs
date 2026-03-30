use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, Write};
use syntect::{
    easy::HighlightLines,
    highlighting::ThemeSet,
    parsing::SyntaxSet,
    util::as_24_bit_terminal_escaped,
};

/// UTF-8の先頭バイトからそのコードポイントのバイト長を返す
fn utf8_char_len(b: u8) -> usize {
    if b & 0b1111_0000 == 0b1111_0000 {
        4
    } else if b & 0b1110_0000 == 0b1110_0000 {
        3
    } else if b & 0b1100_0000 == 0b1100_0000 {
        2
    } else {
        1
    }
}

/// ANSIエスケープシーケンスを壊さない単位で文字列を分割する。
/// Rawモード対応のため、改行 \n は \r\n に正規化する。
fn split_into_chunks(s: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    // Rawモード対応: \n → \r\n に置換（\r\n はそのまま）
    let normalized = {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\r' {
                out.push('\r');
                if chars.peek() == Some(&'\n') {
                    out.push('\n');
                    chars.next();
                }
            } else if c == '\n' {
                out.push('\r');
                out.push('\n');
            } else {
                out.push(c);
            }
        }
        out
    };

    let bytes = normalized.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == 0x1b {
            // ESCシーケンス開始。連続する複数のシーケンスをまとめて消費
            let mut chunk = String::new();
            while i < len && bytes[i] == 0x1b {
                chunk.push('\x1b');
                i += 1;
                if i < len && bytes[i] == b'[' {
                    chunk.push('[');
                    i += 1;
                    while i < len && bytes[i] != b'm' {
                        chunk.push(char::from(bytes[i]));
                        i += 1;
                    }
                    if i < len && bytes[i] == b'm' {
                        chunk.push('m');
                        i += 1;
                    }
                }
            }
            // シーケンス直後の印字文字（UTF-8, 1コードポイント）をチャンクに同梱
            if i < len {
                // \r\n のペアはまとめて1チャンクにする
                if bytes[i] == b'\r' {
                    chunk.push('\r');
                    i += 1;
                    if i < len && bytes[i] == b'\n' {
                        chunk.push('\n');
                        i += 1;
                    }
                } else {
                    let char_len = utf8_char_len(bytes[i]);
                    if i + char_len <= len {
                        if let Ok(c) = std::str::from_utf8(&bytes[i..i + char_len]) {
                            chunk.push_str(c);
                            i += char_len;
                        } else {
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            chunks.push(chunk);
        } else if bytes[i] == b'\r' {
            // \r\n ペアを1チャンクとして扱う
            let mut chunk = String::from("\r");
            i += 1;
            if i < len && bytes[i] == b'\n' {
                chunk.push('\n');
                i += 1;
            }
            chunks.push(chunk);
        } else {
            // 通常のUTF-8文字
            let char_len = utf8_char_len(bytes[i]);
            if i + char_len <= len {
                if let Ok(c) = std::str::from_utf8(&bytes[i..i + char_len]) {
                    chunks.push(c.to_string());
                    i += char_len;
                } else {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
    }

    chunks
}

fn build_chunks(source: &str) -> Result<Vec<String>> {
    let ss = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();

    let syntax = ss
        .find_syntax_by_extension("rs")
        .context("Rust syntax not found in syntect defaults")?;

    let theme_names = ["base16-ocean.dark", "base16-mocha.dark", "InspiredGitHub"];
    let theme = theme_names
        .iter()
        .find_map(|name| ts.themes.get(*name))
        .unwrap_or_else(|| ts.themes.values().next().expect("no themes loaded"));

    let mut h = HighlightLines::new(syntax, theme);
    let mut raw_chunks: Vec<String> = Vec::new();

    for line in syntect::util::LinesWithEndings::from(source) {
        let ranges = h
            .highlight_line(line, &ss)
            .context("highlight_line failed")?;
        let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
        let line_str = format!("{}\x1b[0m", escaped);
        raw_chunks.extend(split_into_chunks(&line_str));
    }

    // --- 空白チャンクを直後の実文字チャンクに結合する ---
    // 「空白のみ」かどうかを判定するヘルパー
    // ANSIエスケープを除去した上で空白・改行だけかチェックする
    fn is_whitespace_chunk(s: &str) -> bool {
        // \x1b[...m を除去して残った文字が全て空白かどうか
        let mut in_escape = false;
        let mut has_visible = false;
        for c in s.chars() {
            if c == '\x1b' {
                in_escape = true;
                continue;
            }
            if in_escape {
                if c == 'm' {
                    in_escape = false;
                }
                continue;
            }
            // エスケープ外の文字
            if !c.is_whitespace() {
                has_visible = true;
                break;
            }
        }
        !has_visible
    }

    // 空白チャンクを「次の実文字チャンクの先頭」に結合する
    let mut merged: Vec<String> = Vec::new();
    let mut pending = String::new(); // 空白チャンクの蓄積バッファ

    for chunk in raw_chunks {
        if is_whitespace_chunk(&chunk) {
            // 空白系はそのまま蓄積
            pending.push_str(&chunk);
        } else {
            // 実文字チャンクが来たら、蓄積した空白を先頭に結合して追加
            let mut combined = pending.clone();
            combined.push_str(&chunk);
            merged.push(combined);
            pending.clear();
        }
    }
    // 末尾に空白だけ残った場合は最後のチャンクに追加（または捨てる）
    if !pending.is_empty() && !merged.is_empty() {
        if let Some(last) = merged.last_mut() {
            last.push_str(&pending);
        }
    }

    Ok(merged)
}

fn run() -> Result<()> {
    // --- ターゲットファイルの読み込み ---
    let raw_path =
        "~/ghq/github.com/BurntSushi/ripgrep/crates/core/main.rs";
    let expanded = shellexpand::tilde(raw_path).to_string();
    let source = std::fs::read_to_string(&expanded)
        .with_context(|| format!("Failed to read file: {}", expanded))?;

    // --- シンタックスハイライト済みチャンクの構築 ---
    let chunks = build_chunks(&source)?;
    if chunks.is_empty() {
        anyhow::bail!("No content to display.");
    }

    // --- ターミナル初期化 ---
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, Hide, Clear(ClearType::All), MoveTo(0, 0))?;

    // --- メインループ ---
    let mut idx: usize = 0;
    let total = chunks.len();

    loop {
        let ev = event::read()?;

        match ev {
            // ESC or Ctrl+C で終了
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                break;
            }

            // 任意のキーで次の1チャンクを出力
            Event::Key(_) => {
                if idx >= total {
                    // 末尾到達 → 先頭にループ
                    execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
                    idx = 0;
                }

                let chunk = &chunks[idx];
                // write! を使うことで \r\n がそのままバイト列として送出される
                write!(stdout, "{}", chunk)?;
                stdout.flush()?;
                idx += 1;
            }

            _ => {}
        }
    }

    // --- ターミナル後片付け ---
    execute!(stdout, Show, LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}