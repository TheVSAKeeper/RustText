use std::{
    fs::{self, File},
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gpui_kit::component::input::Rope;
use rusttext::{
    json_format,
    log_io::{LogError, LogReader, LogSearchQuery, PageOptions, SearchLimits, search_page},
    search::{self, Command, Hit, Outcome, Query, Source},
};

fn main() {
    let tmp = std::env::temp_dir().join(format!("rt_corebench_{}", std::process::id()));
    let _ = fs::create_dir_all(&tmp);
    let result = run_bench(&tmp);
    let _ = fs::remove_dir_all(&tmp);
    if let Err(err) = result {
        eprintln!("core_bench error: {err}");
        std::process::exit(1);
    }
}

fn run_bench(tmp: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let log512 = tmp.join("log512.txt");
    let line_bytes =
        b"2026-11-16T21:30:00Z INFO rusttext::bench line=12345 message=hello_world_test_data\r\n";
    let mut block = Vec::with_capacity(4 * 1024 * 1024);
    while block.len() + line_bytes.len() <= 4 * 1024 * 1024 {
        block.extend_from_slice(line_bytes);
    }
    {
        let mut f = File::create(&log512)?;
        for _ in 0..128 {
            f.write_all(&block)?;
        }
        f.sync_all()?;
    }

    let cancel_none = AtomicBool::new(false);

    // Warmup OS cache on log512
    {
        let mut reader = LogReader::open(&log512, None).map_err(log_err)?;
        let cap = reader.size().min(16 * 1024 * 1024);
        let _ = reader.advance_index(cap, &cancel_none);
    }

    // 1. LineIndex on 512 MiB
    let t0 = Instant::now();
    let (file_size, lines_count) = {
        let mut reader = LogReader::open(&log512, None).map_err(log_err)?;
        let size = reader.size();
        reader.advance_index(size, &cancel_none).map_err(log_err)?;
        (size, reader.line_index().line_count())
    };
    let elapsed_index = t0.elapsed();
    let index_gb_sec =
        (file_size as f64 / (1024.0 * 1024.0 * 1024.0)) / elapsed_index.as_secs_f64();
    println!(
        "LineIndex(512MiB): {:.1} ms ({index_gb_sec:.2} GiB/s), lines={lines_count}",
        elapsed_index.as_secs_f64() * 1000.0
    );

    // 2. LogSearch literal & regex miss over 512 MiB
    let lit_query = LogSearchQuery::literal("NONEXISTENT_NEEDLE_XYZ", true);
    let regex_query = LogSearchQuery::regex(r"line=\d+.*NONEXISTENT", true);
    let _ = search_page(
        &log512,
        None,
        &lit_query,
        0,
        32,
        SearchLimits {
            max_scanned_bytes: 1024 * 1024,
            preview_bytes: 300,
            ..SearchLimits::default()
        },
        &cancel_none,
    );

    let full_limits = SearchLimits {
        max_scanned_bytes: file_size,
        preview_bytes: 300,
        ..SearchLimits::default()
    };
    let t0 = Instant::now();
    let page1 = search_page(&log512, None, &lit_query, 0, 32, full_limits, &cancel_none)
        .map_err(log_err)?;
    let elapsed_lit = t0.elapsed();
    let lit_mb_sec = (page1.scanned_bytes as f64 / 1_048_576.0) / elapsed_lit.as_secs_f64();
    println!(
        "LogSearchLiteralMiss({:.0}MiB): {:.1} ms ({lit_mb_sec:.1} MiB/s)",
        page1.scanned_bytes as f64 / 1_048_576.0,
        elapsed_lit.as_secs_f64() * 1000.0
    );

    let t0 = Instant::now();
    let page2 = search_page(
        &log512,
        None,
        &regex_query,
        0,
        32,
        full_limits,
        &cancel_none,
    )
    .map_err(log_err)?;
    let elapsed_reg = t0.elapsed();
    let reg_mb_sec = (page2.scanned_bytes as f64 / 1_048_576.0) / elapsed_reg.as_secs_f64();
    println!(
        "LogSearchRegexMiss({:.0}MiB): {:.1} ms ({reg_mb_sec:.1} MiB/s)",
        page2.scanned_bytes as f64 / 1_048_576.0,
        elapsed_reg.as_secs_f64() * 1000.0
    );

    // 3. EditorSearch on 40 MiB Rope
    let mut json40 = String::with_capacity(42 * 1024 * 1024);
    json40.push_str("[\n");
    for _ in 0..450_000 {
        json40.push_str(
            "  {\"id\": 12345, \"level\": \"INFO\", \"msg\": \"hello_world_test_data\", \"ok\": true},\n",
        );
    }
    json40.push_str("  {\"id\": 0}\n]");
    let rope = Rope::from(json40.as_str());
    let es_query = Query {
        text: "hello_world_test_data".into(),
        regex: false,
        case_sensitive: false,
        whole_word: false,
        all_tabs: false,
    };
    let origin = Hit {
        tab: 1,
        range: 0..0,
    };
    let _ = search::run(
        &es_query,
        Command::Next,
        vec![Source {
            tab: 1,
            text: rope.clone(),
        }],
        &origin,
        false,
        "",
    );

    let t0 = Instant::now();
    let es_outcome = search::run(
        &es_query,
        Command::Next,
        vec![Source {
            tab: 1,
            text: rope.clone(),
        }],
        &origin,
        false,
        "",
    )
    .map_err(std::io::Error::other)?;
    let elapsed_es = t0.elapsed();
    let es_hits = match es_outcome {
        Outcome::Found { total, .. } => total,
        _ => 0,
    };
    println!(
        "EditorSearch(40MiB): {:.1} ms, hits={es_hits}",
        elapsed_es.as_secs_f64() * 1000.0
    );

    // 4. JsonFormat on 40 MiB
    let _ = json_format::format("{\"a\":1}", "\n");
    let t0 = Instant::now();
    let formatted_json = json_format::format(&json40, "\n").map_err(std::io::Error::other)?;
    let elapsed_jf = t0.elapsed();
    println!(
        "JsonFormat(40MiB): {:.1} ms, ok=True, outLen={}",
        elapsed_jf.as_secs_f64() * 1000.0,
        formatted_json.len()
    );

    // 5. Log random jump (50% & EOF) + page scroll
    {
        let mut jump_reader = LogReader::open(&log512, None).map_err(log_err)?;
        jump_reader
            .advance_index(file_size, &cancel_none)
            .map_err(log_err)?;
        let opts = PageOptions {
            max_rows: 60,
            max_row_bytes: 2048,
        };
        let _ = jump_reader.page_at_line(file_size / 2, opts);
        let t0 = Instant::now();
        let mid_page = jump_reader
            .page_at_line(file_size / 2, opts)
            .map_err(log_err)?;
        let end_page = jump_reader
            .page_before(file_size, opts, 4 * 1024 * 1024)
            .map_err(log_err)?;
        let next_page = jump_reader
            .render_page(mid_page.next_offset, opts)
            .map_err(log_err)?;
        let elapsed_jump = t0.elapsed();
        println!(
            "LogJumpAndScroll(512MiB): {:.2} ms (50%={} rows, EOF={} rows, next={} rows)",
            elapsed_jump.as_secs_f64() * 1000.0,
            mid_page.rows.len(),
            end_page.rows.len(),
            next_page.rows.len()
        );
    }

    // 6. Log search cancellation latency
    {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_bg = cancel.clone();
        let path_bg = log512.clone();
        let regex_bg = regex_query.clone();
        let handle = std::thread::spawn(move || {
            let _ = search_page(
                &path_bg,
                None,
                &regex_bg,
                0,
                32,
                SearchLimits {
                    max_scanned_bytes: file_size,
                    preview_bytes: 300,
                    ..SearchLimits::default()
                },
                &cancel_bg,
            );
        });
        std::thread::sleep(Duration::from_millis(25));
        let t0 = Instant::now();
        cancel.store(true, Ordering::Relaxed);
        let _ = handle.join();
        let cancel_ms = t0.elapsed().as_secs_f64() * 1000.0;
        println!("LogSearchCancel: {cancel_ms:.2} ms");
    }

    // 7. Editor ReplaceAll on 40 MiB
    let rep_query = Query {
        text: "hello_world_test_data".into(),
        regex: false,
        case_sensitive: true,
        whole_word: false,
        all_tabs: false,
    };
    let t0 = Instant::now();
    let rep_outcome = search::run(
        &rep_query,
        Command::ReplaceAll,
        vec![Source { tab: 1, text: rope }],
        &origin,
        false,
        "replaced_ok",
    )
    .map_err(std::io::Error::other)?;
    let elapsed_rep = t0.elapsed();
    let rep_count = match rep_outcome {
        Outcome::Replaced { count, .. } => count,
        _ => 0,
    };
    println!(
        "EditorReplaceAll(40MiB): {:.1} ms, count={rep_count}",
        elapsed_rep.as_secs_f64() * 1000.0
    );

    Ok(())
}

fn log_err(err: LogError) -> std::io::Error {
    std::io::Error::other(err.to_string())
}
