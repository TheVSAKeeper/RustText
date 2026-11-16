use std::{sync::atomic::Ordering, time::Instant};

use gpui_kit::{
    component::{
        Disableable, Selectable, Sizable,
        button::{Button, ButtonVariants},
        input::{Input, InputEvent, InputState},
        label::Label,
    },
    *,
};

use super::{LogViewer, Progress, measurement_start, viewport::Destination};
use crate::log_io::{self, LogError, LogSearchQuery, SearchHit, SearchLimits, SearchPage};

const SEARCH_BATCH_BYTES: u64 = 8 * 1024 * 1024;
const SEARCH_PAGE_SIZE: usize = 32;

pub(super) struct LogSearch {
    pub(super) input: Entity<InputState>,
    regex: bool,
    case_sensitive: bool,
    whole_word: bool,
    pub(super) results: Option<SearchPage>,
    message: SharedString,
    pub(super) progress: Option<Progress>,
    pub(super) cancelled_at: Option<Instant>,
    _enter: Subscription,
}

impl LogSearch {
    fn new(window: &mut Window, cx: &mut Context<LogViewer>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Текст в логе"));
        let enter = cx.subscribe_in(&input, window, |viewer, _, event, window, cx| match event {
            InputEvent::PressEnter { .. } => viewer.start_search(false, window, cx),
            InputEvent::Change => {
                viewer.reset_search();
                cx.notify();
            }
            _ => {}
        });
        Self {
            input,
            regex: false,
            case_sensitive: false,
            whole_word: false,
            results: None,
            progress: None,
            message: "Введите текст или шаблон поиска.".into(),
            cancelled_at: None,
            _enter: enter,
        }
    }

    fn render(&self, enabled: bool, cx: &mut Context<LogViewer>) -> impl IntoElement {
        let running = self.progress.is_some();
        let can_continue = self.results.as_ref().is_some_and(|page| !page.complete);
        let message = self.progress.as_ref().map_or_else(
            || self.message.clone(),
            |progress| {
                let done = progress.done.load(Ordering::Relaxed);
                let total = progress.total.load(Ordering::Relaxed);
                if progress.cancel.load(Ordering::Relaxed) {
                    "Остановка поиска…".into()
                } else {
                    format!("Поиск… {}%", progress_percent(done, total)).into()
                }
            },
        );

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_1()
            .px_3()
            .py_2()
            .bg(rgb(0x18202b))
            .border_b_1()
            .border_color(rgb(0x293442))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.input).disabled(running)),
                    )
                    .child(
                        Button::new("log-find-new")
                            .small()
                            .ghost()
                            .label("Найти")
                            .disabled(running || !enabled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_search(false, window, cx)
                            })),
                    )
                    .child(
                        Button::new("log-find-more")
                            .small()
                            .ghost()
                            .label("Ещё")
                            .disabled(running || !enabled || !can_continue)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_search(true, window, cx)
                            })),
                    )
                    .child(
                        Button::new("log-find-cancel")
                            .small()
                            .ghost()
                            .label("Отменить поиск")
                            .disabled(!running)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_search(cx))),
                    )
                    .child(
                        Button::new("log-find-close")
                            .small()
                            .ghost()
                            .label("Закрыть поиск")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_search();
                                this.search = None;
                                window.focus(&this.focus_handle(cx), cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(option_button(
                        "log-regex",
                        "Регулярка",
                        self.regex,
                        running,
                        |search| &mut search.regex,
                        cx,
                    ))
                    .child(option_button(
                        "log-case",
                        "С учётом регистра",
                        self.case_sensitive,
                        running,
                        |search| &mut search.case_sensitive,
                        cx,
                    ))
                    .child(option_button(
                        "log-word",
                        "Целое слово",
                        self.whole_word,
                        running,
                        |search| &mut search.whole_word,
                        cx,
                    ))
                    .child(Label::new(message).text_sm().truncate()),
            )
            .children(
                self.results
                    .as_ref()
                    .filter(|page| !page.hits.is_empty())
                    .map(|page| {
                        div()
                            .id("log-search-results")
                            .max_h(px(99.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .children(page.hits.iter().map(|hit| result_button(hit, cx)))
                    }),
            )
    }
}

impl LogViewer {
    pub(crate) fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_none() {
            self.search = Some(LogSearch::new(window, cx));
        }
        if let Some(search) = &self.search {
            search.input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        }
        cx.notify();
    }

    pub(super) fn search_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        self.search
            .as_ref()
            .map(|search| search.render(self.available(), cx))
    }

    pub(super) fn reset_search(&mut self) {
        if let Some(search) = &mut self.search {
            if let Some(progress) = search.progress.take() {
                progress.set_cancelled();
            }
            search.results = None;
            search.cancelled_at = None;
            search.message = "Готово к поиску.".into();
        }
    }

    pub(super) fn cancel_search(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search
            && let Some(progress) = &search.progress
        {
            progress.set_cancelled();
            search.cancelled_at.get_or_insert_with(Instant::now);
            cx.notify();
        }
    }

    pub(super) fn start_search(&mut self, more: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.available() {
            return;
        }
        let Some(search) = &mut self.search else {
            return;
        };
        if search.progress.is_some() {
            return;
        }
        let pattern = search.input.read(cx).value().to_string();
        if pattern.is_empty() {
            search.message = "Введите текст или шаблон поиска.".into();
            cx.notify();
            return;
        }
        let fresh = if search.regex {
            LogSearchQuery::regex(pattern, search.case_sensitive)
        } else {
            LogSearchQuery::literal(pattern, search.case_sensitive)
        };
        let query = LogSearchQuery {
            whole_word: search.whole_word,
            ..fresh
        };
        let offset = if more {
            search
                .results
                .as_ref()
                .map_or(self.bookmark.hidden, |page| page.next_offset)
        } else {
            search.results = None;
            self.bookmark.hidden
        };
        search.cancelled_at = None;
        let size = self.bookmark.size;
        let progress = Progress::new(size.saturating_sub(offset));
        let cancel = progress.cancel.clone();
        let done = progress.done.clone();
        search.progress = Some(progress);

        let path = self.path.clone();
        let encoding = self.encoding_override;
        let operation = cx.background_spawn({
            let cancel = cancel.clone();
            async move {
                let limits = SearchLimits {
                    max_scanned_bytes: SEARCH_BATCH_BYTES,
                    preview_bytes: 300,
                    ..SearchLimits::default()
                };
                let mut position = offset;
                loop {
                    let page = log_io::search_page(
                        &path,
                        encoding,
                        &query,
                        position,
                        SEARCH_PAGE_SIZE,
                        limits,
                        &cancel,
                    )?;
                    done.store(page.next_offset.saturating_sub(offset), Ordering::Relaxed);
                    if page.complete || !page.hits.is_empty() || page.next_offset <= position {
                        return Ok(page);
                    }
                    position = page.next_offset;
                }
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = operation.await;
            let _ = this.update_in(cx, |viewer, window, cx| {
                let Some(search) = &mut viewer.search else {
                    return;
                };
                if !search
                    .progress
                    .as_ref()
                    .is_some_and(|progress| std::sync::Arc::ptr_eq(&progress.cancel, &cancel))
                {
                    return;
                }
                search.progress = None;
                if let Some(at) = search.cancelled_at.take() {
                    if measurement_start().is_some() {
                        println!("RUSTTEXT cancel_search_ms={}", at.elapsed().as_millis());
                    }
                    search.message = "Поиск отменён.".into();
                    cx.notify();
                    if super::e2e_enabled() {
                        viewer.advance_e2e(window, cx);
                    }
                    return;
                }
                match result {
                    Ok(page) => {
                        search.message = if page.hits.is_empty() {
                            "Совпадений больше нет.".into()
                        } else if page.complete {
                            format!(
                                "Строк с совпадениями: {}. Достигнут конец файла.",
                                page.hits.len()
                            )
                            .into()
                        } else {
                            format!(
                                "Строк с совпадениями: {}. Нажмите «Ещё» для продолжения.",
                                page.hits.len()
                            )
                            .into()
                        };
                        search.results = Some(page);
                    }
                    Err(LogError::Cancelled) => search.message = "Поиск отменён.".into(),
                    Err(error) => search.message = format!("Поиск остановлен. {error}").into(),
                }
                cx.notify();
                if super::e2e_enabled() {
                    viewer.advance_e2e(window, cx);
                }
            });
        })
        .detach();
        cx.notify();
    }
}

pub(super) fn progress_percent(done: u64, total: u64) -> u64 {
    done.saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
        .min(100)
}

fn option_button(
    id: &'static str,
    label: &'static str,
    selected: bool,
    disabled: bool,
    field: fn(&mut LogSearch) -> &mut bool,
    cx: &mut Context<LogViewer>,
) -> Button {
    Button::new(id)
        .small()
        .outline()
        .label(label)
        .selected(selected)
        .disabled(disabled)
        .on_click(cx.listener(move |this, _, _, cx| {
            if let Some(search) = &mut this.search {
                let value = field(search);
                *value = !*value;
                this.reset_search();
                cx.notify();
            }
        }))
}

fn result_button(hit: &SearchHit, cx: &mut Context<LogViewer>) -> Button {
    let offset = hit.line_start;
    Button::new(("log-hit", offset))
        .small()
        .ghost()
        .w_full()
        .label(format!("Байт {offset}: {}", hit.preview))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.navigate(Destination::Match(offset), window, cx)
        }))
}
