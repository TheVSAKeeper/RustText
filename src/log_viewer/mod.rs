mod controls;
mod file_actions;
mod find;
mod index;
mod viewport;

use std::{
    cell::Cell,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use gpui_kit::{
    component::input::{EditorState, InputEvent, InputState},
    *,
};

use crate::{
    log_file::{LogBookmark, Observation},
    log_io::{LineIndex, Page},
    text_format::TextEncoding,
};

use self::{find::LogSearch, viewport::Destination};

actions!(
    log_viewer,
    [LogStart, LogEnd, LogPrevious, LogNext, LogFind]
);

pub(crate) fn bind_keys(cx: &mut App) {
    let text_area = Some("LogViewer > (Input && mode == editor)");
    cx.bind_keys([
        KeyBinding::new("ctrl-home", LogStart, text_area),
        KeyBinding::new("ctrl-end", LogEnd, text_area),
        KeyBinding::new("pageup", LogPrevious, text_area),
        KeyBinding::new("pagedown", LogNext, text_area),
        KeyBinding::new("ctrl-f", LogFind, Some("LogViewer")),
    ]);
}

struct Progress {
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
}

impl Progress {
    fn new(total: u64) -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicU64::new(0)),
            total: Arc::new(AtomicU64::new(total)),
        }
    }

    fn set_cancelled(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub(crate) struct LogViewer {
    path: PathBuf,
    encoding_override: Option<TextEncoding>,
    encoding: TextEncoding,
    bookmark: LogBookmark,
    observation: Option<Observation>,
    page: Option<Page>,
    index: LineIndex,
    index_paused: bool,
    indexing: Option<Progress>,
    viewer: Entity<EditorState>,
    scroll_bottom: Rc<Cell<Pixels>>,
    wrap: bool,
    percentage: Entity<InputState>,
    trim_percentage: Entity<InputState>,
    search: Option<LogSearch>,
    notice: Option<SharedString>,
    reading: bool,
    generation: u64,
    preparing_trim: bool,
    preparation: Option<Progress>,
    trimming: Option<Progress>,
    e2e_stage: u8,
    e2e_first_painted: bool,
    e2e_await_paint: bool,
    e2e_stage_at: Option<Instant>,
    _submit_percentage: Subscription,
}

impl LogViewer {
    pub(crate) fn new(
        path: PathBuf,
        windows_1251: bool,
        bookmark: LogBookmark,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let encoding_override = windows_1251.then_some(TextEncoding::Windows1251);
        let log_viewer = cx.entity().downgrade();
        let scroll_bottom = Rc::new(Cell::new(px(0.)));
        let editor_scroll_bottom = scroll_bottom.clone();
        let viewer = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx)
                .language("text")
                .line_number(false)
                .searchable(false)
                .scroll_beyond_last_line(Some(0))
                .soft_wrap(true);
            editor.set_scrollbar_renderer(Some(Rc::new(move |scroll, _, _| {
                Self::scrollbar(scroll, log_viewer.clone(), &editor_scroll_bottom)
                    .into_any_element()
            })));
            editor
        });
        let percentage = cx.new(|cx| InputState::new(window, cx).placeholder("0–100"));
        let trim_percentage = cx.new(|cx| InputState::new(window, cx).default_value("50"));
        let submit_percentage =
            cx.subscribe_in(&percentage, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.jump_percent(window, cx);
                }
            });
        let mut this = Self {
            path,
            encoding_override,
            encoding: encoding_override.unwrap_or_default(),
            bookmark,
            observation: None,
            page: None,
            index: LineIndex::default(),
            index_paused: false,
            indexing: None,
            viewer,
            scroll_bottom,
            wrap: true,
            percentage,
            trim_percentage,
            search: None,
            notice: None,
            reading: false,
            generation: 0,
            preparing_trim: false,
            preparation: None,
            trimming: None,
            e2e_stage: u8::from(e2e_enabled()),
            e2e_first_painted: false,
            e2e_await_paint: false,
            e2e_stage_at: None,
            _submit_percentage: submit_percentage,
        };
        this.load_page(Destination::Restore, window, cx);
        this.start_monitor(window, cx);
        this
    }

    pub(crate) fn bookmark(&self) -> LogBookmark {
        self.bookmark.clone()
    }

    pub(crate) fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.viewer.read(cx).focus_handle(cx)
    }

    pub(crate) fn wraps_lines(&self) -> bool {
        self.wrap
    }

    pub(crate) fn toggle_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.wrap = !self.wrap;
        self.viewer
            .update(cx, |editor, cx| editor.set_soft_wrap(self.wrap, window, cx));
        cx.notify();
    }

    pub(crate) fn writing_file(&self) -> bool {
        self.trimming.is_some()
    }

    fn available(&self) -> bool {
        self.observation.is_some() && !self.preparing_trim && !self.writing_file()
    }

    fn start_monitor(&self, window: &mut Window, cx: &mut Context<Self>) {
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut cycle = 0_u8;
            loop {
                executor.timer(Duration::from_millis(125)).await;
                cycle = (cycle + 1) % 4;
                let Ok(poll) = this.update_in(cx, |viewer, _, cx| {
                    if viewer.indexing.is_some()
                        || viewer.preparation.is_some()
                        || viewer.writing_file()
                        || viewer
                            .search
                            .as_ref()
                            .is_some_and(|search| search.progress.is_some())
                    {
                        cx.notify();
                    }
                    if cycle != 0
                        || !viewer.bookmark.auto_refresh
                        || viewer.reading
                        || viewer.preparing_trim
                        || viewer.writing_file()
                    {
                        return None;
                    }
                    Some((
                        viewer.path.clone(),
                        viewer.observation.clone(),
                        viewer.generation,
                    ))
                }) else {
                    break;
                };
                if let Some((path, previous, generation)) = poll {
                    let observed = executor
                        .spawn(async move { crate::log_file::observe(&path, previous.as_ref()) })
                        .await;
                    let _ = this.update_in(cx, |viewer, window, cx| {
                        if viewer.generation == generation && viewer.bookmark.auto_refresh {
                            viewer.apply_observation(observed, window, cx);
                        }
                    });
                }
            }
        })
        .detach();
    }

    pub(super) fn on_e2e_frame_presented(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !e2e_enabled() {
            return;
        }
        self.e2e_first_painted = true;
        if !self.reading && self.page.is_some() {
            self.e2e_await_paint = false;
        }
        self.advance_e2e(window, cx);
    }

    pub(super) fn advance_e2e(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !e2e_enabled() {
            return;
        }
        match self.e2e_stage {
            1 => {
                let ready = self.e2e_first_painted
                    && !self.reading
                    && self.page.is_some()
                    && self
                        .observation
                        .as_ref()
                        .is_some_and(|obs| self.index.scanned_to() >= obs.stamp.len)
                    && self.indexing.is_none();
                if !ready {
                    return;
                }
                self.e2e_stage = 2;
                self.e2e_await_paint = true;
                self.e2e_stage_at = Some(Instant::now());
                self.percentage
                    .update(cx, |input, cx| input.set_value("50", window, cx));
                self.jump_percent(window, cx);
            }
            2 => {
                if self.e2e_await_paint || self.reading {
                    return;
                }
                if let Some(at) = self.e2e_stage_at.take() {
                    println!("RUSTTEXT jump_50pct_ms={}", at.elapsed().as_millis());
                }
                self.e2e_stage = 3;
                self.e2e_await_paint = true;
                self.e2e_stage_at = Some(Instant::now());
                self.navigate(Destination::End, window, cx);
            }
            3 => {
                if self.e2e_await_paint || self.reading {
                    return;
                }
                if let Some(at) = self.e2e_stage_at.take() {
                    println!("RUSTTEXT jump_eof_ms={}", at.elapsed().as_millis());
                }
                self.e2e_stage = 4;
                self.e2e_await_paint = true;
                self.e2e_stage_at = Some(Instant::now());
                let before = self.bookmark.offset;
                self.navigate(Destination::Before(before), window, cx);
            }
            4 => {
                if self.e2e_await_paint || self.reading {
                    return;
                }
                if let Some(at) = self.e2e_stage_at.take() {
                    println!("RUSTTEXT scroll_prev_ms={}", at.elapsed().as_millis());
                }
                if let Some(needle) = e2e_needle() {
                    self.e2e_stage = 5;
                    self.e2e_stage_at = Some(Instant::now());
                    self.open_search(window, cx);
                    if let Some(search) = &self.search {
                        search
                            .input
                            .update(cx, |input, cx| input.set_value(needle, window, cx));
                    }
                    self.start_search(false, window, cx);
                } else {
                    self.start_e2e_cancel_search(window, cx);
                }
            }
            5 => {
                if self
                    .search
                    .as_ref()
                    .is_some_and(|search| search.progress.is_some())
                {
                    return;
                }
                if let Some(at) = self.e2e_stage_at.take() {
                    println!("RUSTTEXT search_eof_ms={}", at.elapsed().as_millis());
                }
                let (hits, complete) = self
                    .search
                    .as_ref()
                    .and_then(|s| s.results.as_ref())
                    .map_or((0, false), |r| (r.hits.len(), r.complete));
                println!("RUSTTEXT search_eof_hits={hits}");
                println!("RUSTTEXT search_eof_complete={}", u8::from(complete));
                self.start_e2e_cancel_search(window, cx);
            }
            6 => {
                let done = self.search.as_ref().is_none_or(|search| {
                    search.progress.is_none() && search.cancelled_at.is_none()
                });
                if !done {
                    return;
                }
                self.e2e_stage = 7;
                record_memory("after_ops");
                println!("RUSTTEXT e2e_done=1");
                window.remove_window();
                cx.quit();
            }
            _ => {}
        }
    }

    fn start_e2e_cancel_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.e2e_stage = 6;
        self.open_search(window, cx);
        if let Some(search) = &self.search {
            search.input.update(cx, |input, cx| {
                input.set_value("__E2E_MISS_PATTERN__", window, cx)
            });
        }
        self.start_search(false, window, cx);
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |this, cx| {
            executor.timer(Duration::from_millis(25)).await;
            let _ = this.update_in(cx, |viewer, _, cx| {
                if viewer.e2e_stage == 6 {
                    viewer.cancel_search(cx);
                }
            });
        })
        .detach();
    }
}

impl Drop for LogViewer {
    fn drop(&mut self) {
        self.stop_index();
        if let Some(progress) = &self.preparation {
            progress.set_cancelled();
        }
        if let Some(progress) = &self.trimming {
            progress.set_cancelled();
        }
        if let Some(search) = &self.search
            && let Some(progress) = &search.progress
        {
            progress.set_cancelled();
        }
    }
}

static MEASUREMENT_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static E2E_ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
static E2E_NEEDLE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
static WINDOW_VISIBLE_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static FIRST_FRAME_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static DOC_READY_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static INDEX_READY_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn enable_measurements() {
    let measure = std::env::var_os("RUSTTEXT_MEASURE").is_some();
    let e2e = std::env::var_os("RUSTTEXT_E2E_PROBE").is_some_and(|v| !v.is_empty());
    E2E_ENABLED.set(e2e).ok();
    E2E_NEEDLE
        .set(
            std::env::var("RUSTTEXT_E2E_NEEDLE")
                .ok()
                .filter(|s| !s.is_empty()),
        )
        .ok();
    if measure || e2e {
        MEASUREMENT_START.set(std::time::Instant::now()).ok();
    }
}

pub(crate) fn measurement_start() -> Option<std::time::Instant> {
    MEASUREMENT_START.get().copied()
}

pub(crate) fn e2e_enabled() -> bool {
    E2E_ENABLED.get().copied().unwrap_or(false)
}

pub(crate) fn e2e_needle() -> Option<&'static str> {
    E2E_NEEDLE.get().and_then(|v| v.as_deref())
}

pub(crate) fn record_window_visible() {
    if let Some(start) = measurement_start()
        && !WINDOW_VISIBLE_LOGGED.swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        println!("RUSTTEXT window_visible_ms={}", start.elapsed().as_millis());
    }
}

pub(crate) fn record_first_frame_presented() {
    if let Some(start) = measurement_start()
        && !FIRST_FRAME_LOGGED.swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        println!(
            "RUSTTEXT first_frame_presented_ms={}",
            start.elapsed().as_millis()
        );
    }
}

pub(crate) fn on_after_frame_presented(
    window: &mut Window,
    callback: impl FnOnce(&mut Window, &mut App) + 'static,
) {
    window.on_next_frame(move |window, _| {
        window.on_next_frame(callback);
    });
}

pub(crate) fn record_document_ready() {
    if let Some(start) = measurement_start()
        && !DOC_READY_LOGGED.swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        let ms = start.elapsed().as_millis();
        println!("RUSTTEXT document_ready_ms={ms}");
        println!("RUSTTEXT editor_ready_ms={ms}");
    }
}

pub(crate) fn record_index_ready() {
    if let Some(start) = measurement_start()
        && !INDEX_READY_LOGGED.swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        println!("RUSTTEXT index_ready_ms={}", start.elapsed().as_millis());
        if e2e_enabled() {
            record_memory("index_ready");
        }
    }
}

pub(crate) fn record_memory(stage: &str) {
    if measurement_start().is_none() {
        return;
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        #[allow(non_snake_case)]
        struct ProcessMemoryCountersEx {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_non_paged_pool_usage: usize,
            quota_non_paged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
            private_usage: usize,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn K32GetProcessMemoryInfo(
                process: *mut core::ffi::c_void,
                ppsmem_counters: *mut ProcessMemoryCountersEx,
                cb: u32,
            ) -> i32;
        }
        let mut counters = ProcessMemoryCountersEx {
            cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
            page_fault_count: 0,
            peak_working_set_size: 0,
            working_set_size: 0,
            quota_peak_paged_pool_usage: 0,
            quota_paged_pool_usage: 0,
            quota_peak_non_paged_pool_usage: 0,
            quota_non_paged_pool_usage: 0,
            pagefile_usage: 0,
            peak_pagefile_usage: 0,
            private_usage: 0,
        };
        let handle = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                handle.0,
                &mut counters,
                std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
            )
        };
        if ok != 0 {
            let ws = counters.working_set_size as f64 / 1_048_576.0;
            let peak_ws = counters.peak_working_set_size as f64 / 1_048_576.0;
            let priv_mib = counters.private_usage as f64 / 1_048_576.0;
            println!(
                "RUSTTEXT mem_{stage}_ws_mib={ws:.2} mem_{stage}_peak_ws_mib={peak_ws:.2} mem_{stage}_priv_mib={priv_mib:.2}"
            );
        }
    }
}
