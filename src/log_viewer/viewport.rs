use std::io;

use gpui_kit::*;

use super::LogViewer;
use crate::{
    log_file::{self, Change, LogBookmark, Observation},
    log_io::{LineIndex, LogReader, Page, PageOptions},
    text_format::TextEncoding,
};

const MAX_BACKWARD_SCAN: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Destination {
    Restore,
    Start,
    End,
    At(u64),
    Before(u64),
    Percentage(f64),
    Match(u64),
    Append(bool),
}

struct PreviousPage {
    observation: Option<Observation>,
    page: Option<Page>,
}

struct RenderedPage {
    observation: Observation,
    page: Page,
    start: u64,
    hidden: u64,
    encoding: TextEncoding,
    reset: bool,
    scroll_to_bottom: bool,
}

impl LogViewer {
    pub(super) fn load_page(
        &mut self,
        destination: Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.writing_file() {
            return;
        }
        let path = self.path.clone();
        let bookmark = self.bookmark.clone();
        let encoding = self.encoding_override;
        let previous = PreviousPage {
            observation: self.observation.clone(),
            page: self.page.clone(),
        };
        let index = self.index.clone();
        let rows = ((f32::from(window.viewport_size().height) - 150.0) / 23.0)
            .ceil()
            .clamp(12.0, 120.0) as usize
            + 2;
        let options = PageOptions {
            max_rows: rows,
            max_row_bytes: 2048,
        };
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.reading = true;

        let job = cx.background_spawn(async move {
            read_page(
                &path,
                encoding,
                &bookmark,
                destination,
                options,
                previous,
                index,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |viewer, window, cx| {
                if viewer.generation != generation {
                    return;
                }
                viewer.reading = false;
                match result {
                    Ok(rendered) => viewer.apply_page(rendered, window, cx),
                    Err(error) => {
                        viewer.notice = Some(format!("Не удалось прочитать лог. {error}").into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn apply_page(&mut self, loaded: RenderedPage, window: &mut Window, cx: &mut Context<Self>) {
        let first = self.page.is_none();
        if loaded.reset {
            self.reset_index();
            self.reset_search();
            self.notice
                .get_or_insert("Лог изменился. Позиция и скрытая граница сброшены.".into());
        }
        self.encoding = loaded.encoding;
        self.bookmark.identity = Some(loaded.observation.stamp.identity);
        self.bookmark.size = loaded.observation.stamp.len.max(loaded.page.file_size);
        self.bookmark.offset = loaded.start;
        self.bookmark.hidden = loaded.hidden;
        self.observation = Some(loaded.observation);
        let mut text = String::new();
        for (index, row) in loaded.page.rows.iter().enumerate() {
            if index != 0 {
                text.push('\n');
            }
            text.push_str(&row.text);
        }
        self.viewer.update(cx, |viewer, cx| {
            viewer.set_value(text, window, cx);
            if loaded.scroll_to_bottom {
                // The editor clamps this against the content after laying out any wrapped rows.
                viewer.set_scroll_offset(point(px(0.), px(-f32::MAX)), cx);
            }
        });
        self.page = Some(loaded.page);
        self.start_index(window, cx);

        if (first && crate::log_viewer::measurement_start().is_some())
            || crate::log_viewer::e2e_enabled()
        {
            let start = crate::log_viewer::measurement_start();
            let e2e = crate::log_viewer::e2e_enabled();
            let viewer = cx.entity().downgrade();
            window.on_next_frame(move |window, cx| {
                if first && let Some(start) = start {
                    println!(
                        "RUSTTEXT first_log_frame_ms={}",
                        start.elapsed().as_millis()
                    );
                    if e2e {
                        crate::log_viewer::record_memory("first_frame");
                    }
                }
                if e2e {
                    let _ = viewer.update(cx, |viewer, cx| {
                        viewer.on_e2e_frame_presented(window, cx);
                    });
                }
            });
        }
    }

    pub(super) fn apply_observation(
        &mut self,
        result: io::Result<(Observation, Change)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preparing_trim || self.writing_file() {
            return;
        }
        match result {
            Ok((_, Change::Same)) => {}
            Ok((observation, Change::Appended)) => {
                self.bookmark.size = observation.stamp.len;
                self.observation = Some(observation);
                if self.page.as_ref().is_some_and(|page| page.eof) {
                    self.load_page(Destination::Append(self.bookmark.follow_end), window, cx);
                } else if self.bookmark.follow_end {
                    self.load_page(Destination::End, window, cx);
                } else {
                    self.start_index(window, cx);
                }
                cx.notify();
            }
            Ok((observation, change)) => {
                self.reset_index();
                self.reset_search();
                self.notice = Some(
                    match change {
                        Change::Truncated => "Лог очищен или сокращён. Позиция просмотра сброшена.",
                        Change::Rewritten => {
                            "Содержимое лога изменилось. Позиция просмотра сброшена."
                        }
                        _ => "Файл лога появился или заменён. Позиция просмотра сброшена.",
                    }
                    .into(),
                );
                self.bookmark.hidden = 0;
                self.bookmark.offset = 0;
                self.bookmark.identity = Some(observation.stamp.identity);
                self.bookmark.size = observation.stamp.len;
                self.observation = Some(observation);
                self.load_page(
                    if self.bookmark.follow_end {
                        Destination::End
                    } else {
                        Destination::Start
                    },
                    window,
                    cx,
                );
            }
            Err(error) => {
                let message: SharedString = if error.kind() == io::ErrorKind::NotFound {
                    "Файл лога отсутствует. Ожидание повторного создания.".into()
                } else {
                    format!("Файл лога временно недоступен. {error}").into()
                };
                if self.observation.take().is_some() || self.notice.as_ref() != Some(&message) {
                    self.reset_search();
                    self.reset_index();
                    self.page = None;
                    self.notice = Some(message);
                    self.viewer
                        .update(cx, |viewer, cx| viewer.set_value("", window, cx));
                    cx.notify();
                }
            }
        }
    }

    pub(super) fn jump_percent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.percentage.read(cx).value();
        let percent = input.trim().replace(',', ".").parse::<f64>();
        match percent {
            Ok(percent) if (0.0..=100.0).contains(&percent) => {
                self.navigate(Destination::Percentage(percent), window, cx)
            }
            _ => {
                self.notice = Some("Введите положение от 0 до 100%.".into());
                cx.notify();
            }
        }
    }

    pub(super) fn navigate(
        &mut self,
        destination: Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.available() {
            return;
        }
        self.notice = None;
        self.load_page(destination, window, cx);
    }

    pub(super) fn scroll_screen(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.viewer.read(cx);
        let offset = editor.scroll_offset();
        let edge = if forward {
            self.scroll_bottom.get()
        } else {
            px(0.)
        };
        if (offset.y - edge).abs() <= px(0.5) {
            self.turn_page(forward, window, cx);
            return;
        }
        let line_height = editor.line_height().unwrap_or(window.line_height());
        let step = (editor.input_bounds().size.height - line_height).max(line_height);
        let y = if forward {
            offset.y - step
        } else {
            offset.y + step
        };
        self.viewer.update(cx, |editor, cx| {
            editor.set_scroll_offset(point(offset.x, y), cx)
        });
    }

    pub(super) fn turn_page(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.available() || self.reading {
            return false;
        }
        let destination = if forward {
            let Some(page) = &self.page else {
                return false;
            };
            if page.eof || page.next_offset <= self.bookmark.offset {
                return false;
            }
            Destination::At(page.next_offset)
        } else if self.bookmark.offset > self.bookmark.hidden {
            Destination::Before(self.bookmark.offset)
        } else {
            return false;
        };
        self.navigate(destination, window, cx);
        true
    }
}

fn read_page(
    path: &std::path::Path,
    encoding: Option<TextEncoding>,
    bookmark: &LogBookmark,
    destination: Destination,
    options: PageOptions,
    previous: PreviousPage,
    index: LineIndex,
) -> Result<RenderedPage, String> {
    let mut reader = LogReader::open(path, encoding).map_err(|error| error.to_string())?;
    let mut file = reader
        .file()
        .try_clone()
        .map_err(|error| error.to_string())?;
    let (observation, change) = log_file::observe_file(&mut file, previous.observation.as_ref())
        .map_err(|error| error.to_string())?;
    reader.refresh_size().map_err(|error| error.to_string())?;
    let reset = (bookmark.identity.is_some() && !bookmark.can_restore(&observation.stamp))
        || (previous.observation.is_some()
            && matches!(
                change,
                Change::Replaced | Change::Truncated | Change::Rewritten
            ));
    let floor = if reset {
        0
    } else {
        bookmark.hidden.min(reader.size())
    };
    let start = reader.seek_start().max(floor);
    let destination = if reset && !matches!(destination, Destination::End) {
        Destination::Start
    } else {
        destination
    };

    let scroll_to_bottom = match destination {
        Destination::Restore => bookmark.follow_end && bookmark.auto_refresh,
        Destination::Before(_) | Destination::End | Destination::Append(true) => true,
        Destination::Percentage(percent) => percent >= 100.,
        _ => false,
    };

    let mut page = match destination {
        Destination::Restore if bookmark.follow_end && bookmark.auto_refresh => {
            reader.page_before(reader.size(), options, MAX_BACKWARD_SCAN)
        }
        Destination::Restore => reader.render_page(bookmark.offset.max(start), options),
        Destination::Start => reader.render_page(start, options),
        Destination::End => reader.page_before(reader.size(), options, MAX_BACKWARD_SCAN),
        Destination::At(offset) => reader.render_page(offset.max(start), options),
        Destination::Match(offset) => reader.render_page(offset.max(start), options),
        Destination::Before(offset) => reader.page_before(offset, options, MAX_BACKWARD_SCAN),
        Destination::Percentage(percent) if percent >= 100.0 => {
            reader.page_before(reader.size(), options, MAX_BACKWARD_SCAN)
        }
        Destination::Percentage(percent) => {
            let offset = reader.seek_percent(percent).max(start);
            let sample = index
                .entries()
                .iter()
                .rev()
                .find(|entry| entry.offset <= offset);
            if !reset && sample.is_some_and(|sample| offset - sample.offset <= MAX_BACKWARD_SCAN) {
                reader.set_line_index(index);
                reader.page_at_line(offset, options)
            } else {
                reader.render_page(offset, options)
            }
        }
        Destination::Append(follow) => match previous.page {
            Some(previous) => append_page(&mut reader, previous, options, follow),
            None => reader.render_page(start, options),
        },
    }
    .map_err(|error| error.to_string())?;

    if page.rows.first().is_some_and(|row| row.start < start) {
        page = match destination {
            Destination::Before(offset) => reader.render_page_until(start, offset, options),
            _ => reader.render_page(start, options),
        }
        .map_err(|error| error.to_string())?;
    }

    let start = page
        .rows
        .first()
        .map_or(page.next_offset.max(start), |row| row.start);
    Ok(RenderedPage {
        observation,
        page,
        start,
        hidden: floor,
        encoding: reader.encoding(),
        reset,
        scroll_to_bottom,
    })
}

fn append_page(
    reader: &mut LogReader,
    mut previous: Page,
    options: PageOptions,
    follow: bool,
) -> Result<Page, crate::log_io::LogError> {
    // A partial final line may acquire more text or the terminating newline.
    // Restart at that fragment only; all complete visible rows stay in memory.
    let start = if previous.rows.last().is_some_and(|row| !row.terminated) {
        previous.rows.pop().expect("partial row exists").start
    } else {
        previous.next_offset
    };
    let slots = if follow {
        options.max_rows
    } else {
        options.max_rows.saturating_sub(previous.rows.len())
    };
    if slots == 0 {
        previous.file_size = reader.size();
        previous.eof = previous.next_offset >= previous.file_size;
        return Ok(previous);
    }

    let fresh = reader.render_page(
        start,
        PageOptions {
            max_rows: slots,
            ..options
        },
    )?;
    if follow && !fresh.eof {
        return reader.page_before(reader.size(), options, MAX_BACKWARD_SCAN);
    }
    previous.rows.extend(fresh.rows);
    if previous.rows.len() > options.max_rows && follow {
        let excess = previous.rows.len() - options.max_rows;
        previous.rows.drain(..excess);
    }
    previous.file_size = fresh.file_size;
    previous.eof = fresh.eof;
    previous.next_offset = fresh.next_offset;
    Ok(previous)
}

#[cfg(test)]
#[path = "../../tests/unit/log_viewport.rs"]
mod tests;
