use std::sync::{Arc, atomic::Ordering};

use gpui_kit::*;

use super::{LogViewer, Progress};
use crate::{
    log_file::FileIdentity,
    log_io::{LineIndex, LogError, LogReader},
};

const INDEX_BATCH: u64 = 16 * 1024 * 1024;

impl LogViewer {
    pub(super) fn start_index(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(observation) = &self.observation else {
            return;
        };
        if self.index_paused
            || self.indexing.is_some()
            || self.preparing_trim
            || self.writing_file()
            || self.index.scanned_to() >= observation.stamp.len
        {
            return;
        }
        let size = observation.stamp.len;
        let identity = observation.stamp.identity;
        let path = self.path.clone();
        let encoding = self.encoding_override;
        let progress = Progress::new(size);
        let done = progress.done.clone();
        let cancel = progress.cancel.clone();
        let index = self.index.clone();
        done.store(index.scanned_to(), Ordering::Relaxed);
        self.indexing = Some(progress);
        let job = cx.background_spawn({
            let cancel = cancel.clone();
            async move {
                let mut reader = LogReader::open(path, encoding)?;
                if FileIdentity::read(reader.file())? != identity
                    || reader.size() < index.scanned_to()
                {
                    return Err(LogError::Cancelled);
                }
                reader.set_line_index(index);
                while reader.line_index().scanned_to() < size {
                    let remaining = size - reader.line_index().scanned_to();
                    if reader.advance_index(remaining.min(INDEX_BATCH), &cancel)? == 0 {
                        break;
                    }
                    done.store(reader.line_index().scanned_to(), Ordering::Relaxed);
                }
                Ok(reader.line_index().clone())
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |viewer, window, cx| {
                if !viewer
                    .indexing
                    .as_ref()
                    .is_some_and(|progress| Arc::ptr_eq(&progress.cancel, &cancel))
                {
                    return;
                }
                viewer.indexing = None;
                match result {
                    Ok(index) => {
                        viewer.index = index;
                        if viewer.index.scanned_to() >= size {
                            super::record_index_ready();
                            viewer.start_index(window, cx);
                            if super::e2e_enabled() {
                                viewer.advance_e2e(window, cx);
                            }
                        }
                    }
                    Err(LogError::Cancelled) => {}
                    Err(error) => {
                        viewer.index_paused = true;
                        viewer.notice = Some(format!("Индексирование остановлено. {error}").into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn stop_index(&mut self) {
        if let Some(progress) = self.indexing.take() {
            progress.set_cancelled();
        }
    }

    pub(super) fn reset_index(&mut self) {
        self.stop_index();
        self.index = LineIndex::default();
    }

    pub(super) fn toggle_index(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.index_paused = !self.index_paused;
        if self.index_paused {
            self.stop_index();
        } else {
            self.start_index(window, cx);
        }
        cx.notify();
    }
}
