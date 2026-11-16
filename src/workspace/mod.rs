mod disk_status;
mod file_actions;
mod json_action;
mod launches;
mod search_panel;
mod session_runtime;
mod updates;
mod view;

pub(crate) use view::bind_keys;

use std::path::{Path, PathBuf};

use gpui_kit::component::input::{EditorState, InputEvent, Rope};
use gpui_kit::*;

use crate::{
    files::{FileTarget, OpenedFile},
    log_file::LogBookmark,
    log_viewer::LogViewer,
    recent_files::{RecentFile, RecentFiles},
    session::{MAX_CLOSED_TABS, Tab},
    single_instance::SingleInstance,
    text_format::{TextEncoding, TextFormat},
};

struct Document {
    id: u64,
    name: SharedString,
    file: Option<FileTarget>,
    format: TextFormat,
    editor: Entity<EditorState>,
    saved_text: Option<Rope>,
    dirty: bool,
    revision: u64,
    disk_status: disk_status::DiskStatus,
    placeholder: bool,
    restored_scroll: Option<[f32; 2]>,
    log: Option<Entity<LogViewer>>,
    _subscriptions: [Subscription; 2],
}

struct ClosedTab {
    tab: Tab,
    draft: Option<Rope>,
}

pub(crate) struct Workspace {
    documents: Vec<Document>,
    active: u64,
    next_id: u64,
    tab_scroll: ScrollHandle,
    closed: Vec<ClosedTab>,
    recent: RecentFiles,
    busy: Option<&'static str>,
    error: Option<SharedString>,
    wrap: bool,
    font_size: f32,
    search: Option<search_panel::SearchPanel>,
    session: Option<session_runtime::SessionWriter>,
    session_notice: Option<SharedString>,
    session_error: Option<SharedString>,
    updates: updates::Updates,
}

impl Workspace {
    pub(crate) fn new(
        paths: Vec<PathBuf>,
        startup: crate::session::Startup,
        instance: SingleInstance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (recent, recent_error) = RecentFiles::load();
        let mut this = Self {
            documents: Vec::new(),
            active: 0,
            next_id: 1,
            tab_scroll: ScrollHandle::new(),
            closed: Vec::new(),
            recent,
            busy: None,
            error: recent_error.map(recent_warning),
            wrap: false,
            font_size: 15.0,
            search: None,
            session: None,
            session_notice: None,
            session_error: None,
            updates: updates::Updates::default(),
        };
        this.init_session(startup, window, cx);
        this.listen_for_launches(instance, window, cx);
        let workspace = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            workspace
                .update(cx, |this, cx| this.request_window_close(window, cx))
                .unwrap_or(true)
        });

        if !paths.is_empty() {
            this.open_paths(paths, file_actions::OpenMode::NORMAL, window, cx);
        }
        this.check_for_updates(false, window, cx);
        crate::log_viewer::record_window_visible();
        window.on_next_frame(|_, _| {
            crate::log_viewer::record_first_frame_presented();
        });
        this
    }

    fn active_document(&self) -> &Document {
        self.documents
            .iter()
            .find(|doc| doc.id == self.active)
            .expect("active tab must exist")
    }

    fn add_document(
        &mut self,
        opened: Option<OpenedFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (file, text, format) = match opened {
            Some(opened) => (Some(opened.target), opened.text, opened.format),
            None => (None, String::new(), TextFormat::default()),
        };
        let id = self.insert_document(file, text, format, window, cx);
        self.select(id, window, cx);
    }

    fn insert_document(
        &mut self,
        file: Option<FileTarget>,
        text: String,
        format: TextFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let name = file
            .as_ref()
            .map_or_else(|| "Без имени".into(), |file| file_name(&file.path));
        let language = file
            .as_ref()
            .map_or_else(|| "text".to_owned(), |file| syntax_for(&file.path));

        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(true)
                .searchable(false)
                .soft_wrap(self.wrap)
                .language(language)
                .default_value(text)
        });
        let saved_text = editor.read(cx).text().clone();
        let change = cx.subscribe(&editor, move |this, editor, event, cx| {
            if matches!(event, InputEvent::Change)
                && let Some(doc) = this.documents.iter_mut().find(|doc| doc.id == id)
            {
                doc.dirty = doc.saved_text.as_ref() != Some(editor.read(cx).text());
                doc.revision = doc.revision.wrapping_add(1);
                cx.notify();
            }
        });
        let cursor = cx.observe(&editor, move |this, _, cx| {
            if this.active == id {
                cx.notify();
            }
        });

        self.documents.push(Document {
            id,
            name,
            file,
            format,
            editor,
            saved_text: Some(saved_text),
            dirty: false,
            revision: 0,
            disk_status: disk_status::DiskStatus::Current,
            placeholder: false,
            restored_scroll: None,
            log: None,
            _subscriptions: [change, cursor],
        });
        id
    }

    fn select(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.documents.iter().position(|doc| doc.id == id) {
            if self.active != id {
                self.error = None;
                self.active = id;
            }
            self.tab_scroll.scroll_to_item(index);
            let doc = &self.documents[index];
            let focus = if let Some(log) = &doc.log {
                self.search = None;
                log.read(cx).focus_handle(cx)
            } else {
                doc.editor.read(cx).focus_handle(cx)
            };
            window.focus(&focus, cx);
            cx.notify();
        }
    }

    fn insert_log(
        &mut self,
        target: FileTarget,
        windows_1251: bool,
        bookmark: LogBookmark,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        let path = target.path.clone();
        let format = TextFormat::for_encoding(if windows_1251 {
            TextEncoding::Windows1251
        } else {
            TextEncoding::Utf8
        });
        let id = self.insert_document(Some(target), String::new(), format, window, cx);
        let viewer = cx.new(|cx| LogViewer::new(path, windows_1251, bookmark, window, cx));
        self.documents.last_mut().expect("log tab inserted").log = Some(viewer);
        id
    }

    fn log_is_writing(&self, cx: &App) -> bool {
        self.documents.iter().any(|doc| {
            doc.log
                .as_ref()
                .is_some_and(|log| log.read(cx).writing_file())
        })
    }

    fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.applying_update() {
            return;
        }
        self.error = None;
        self.add_document(None, window, cx);
    }

    fn close_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(doc) = self.documents.iter().find(|doc| doc.id == id) else {
            return;
        };
        if doc
            .log
            .as_ref()
            .is_some_and(|log| log.read(cx).writing_file())
        {
            return;
        }
        if !doc.dirty || (doc.file.is_none() && self.session.is_some()) {
            self.remove_tab(id, window, cx);
            return;
        }

        self.confirm_discard(Some(id), window, cx);
    }

    fn remove_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.documents.iter().position(|doc| doc.id == id) else {
            return;
        };
        let doc = self.documents.remove(index);
        let (tab, draft) = doc.session_snapshot(doc.file.is_none(), cx);
        if self.closed.len() == MAX_CLOSED_TABS {
            self.closed.remove(0);
        }
        self.closed.push(ClosedTab { tab, draft });

        if self.documents.is_empty() {
            self.add_document(None, window, cx);
        } else if id == self.active {
            let next = self.documents[index.min(self.documents.len() - 1)].id;
            self.select(next, window, cx);
        } else {
            cx.notify();
        }
    }

    fn reopen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.closed.last() {
            self.reopen_closed_tab(entry.tab.id, window, cx);
        }
    }

    fn closed_tab_entries(&self) -> Vec<(u64, String)> {
        self.closed
            .iter()
            .rev()
            .map(|entry| {
                let label = if let Some(file) = &entry.tab.file {
                    display_path(&file.path)
                } else {
                    let preview: String = entry
                        .draft
                        .iter()
                        .flat_map(|draft| draft.chunks())
                        .flat_map(str::chars)
                        .take(60)
                        .map(|ch| if ch.is_whitespace() { ' ' } else { ch })
                        .collect();
                    let preview = preview.trim();
                    if preview.is_empty() {
                        "Без имени".to_owned()
                    } else {
                        format!("Без имени — {preview}")
                    }
                };
                (entry.tab.id, label)
            })
            .collect()
    }

    fn reopen_closed_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(index) = self.closed.iter().position(|entry| entry.tab.id == id) else {
            return;
        };
        let tab = &self.closed[index].tab;
        if let Some(file) = &tab.file {
            let mut recent = RecentFile::new(file.path.clone(), tab.format.encoding);
            recent.view_as_log = tab.log.is_some();
            self.open_closed_file(id, recent, window, cx);
        } else {
            let ClosedTab { tab, draft } = self.closed.remove(index);
            let id = self.restore_tab(tab, draft.map(|draft| draft.to_string()), window, cx);
            self.select(id, window, cx);
        }
    }

    fn request_window_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.busy.is_some() || self.log_is_writing(cx) {
            return false;
        }
        if self.session.is_some() {
            return self.begin_session_shutdown(session_runtime::AfterShutdown::Close, window, cx);
        }
        if self.documents.iter().any(|doc| doc.dirty) {
            self.confirm_discard(None, window, cx);
            return false;
        }
        true
    }

    fn confirm_discard(&mut self, tab: Option<u64>, window: &mut Window, cx: &mut Context<Self>) {
        let detail = tab
            .and_then(|id| self.documents.iter().find(|doc| doc.id == id))
            .map_or_else(
                || "В открытых вкладках есть несохранённый текст.".to_owned(),
                |doc| format!("Вкладка «{}» содержит несохранённый текст.", doc.name),
            );
        let answer = window.prompt(
            PromptLevel::Warning,
            "Закрыть без сохранения?",
            Some(&detail),
            &[PromptButton::cancel("Отмена"), PromptButton::new("Закрыть")],
            cx,
        );

        self.busy = Some("Подтверждение закрытия…");
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let discard = answer.await.ok() == Some(1);
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                if discard {
                    if let Some(id) = tab {
                        this.remove_tab(id, window, cx);
                    } else {
                        window.remove_window();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn reorder_tabs(&mut self, dragged_id: u64, target_id: u64, cx: &mut Context<Self>) {
        if self.applying_update() {
            return;
        }
        let from = self.documents.iter().position(|doc| doc.id == dragged_id);
        let to = self.documents.iter().position(|doc| doc.id == target_id);
        if let (Some(from), Some(to)) = (from, to)
            && from != to
        {
            let tab = self.documents.remove(from);
            self.documents.insert(to, tab);
            cx.notify();
        }
    }

    fn toggle_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(viewer) = self.active_document().log.clone() {
            viewer.update(cx, |viewer, cx| viewer.toggle_wrap(window, cx));
            cx.notify();
            return;
        }
        self.wrap = !self.wrap;
        for doc in &self.documents {
            doc.editor
                .update(cx, |editor, cx| editor.set_soft_wrap(self.wrap, window, cx));
        }
        cx.notify();
    }

    fn zoom(&mut self, amount: f32, cx: &mut Context<Self>) {
        self.font_size = (self.font_size + amount).clamp(9.0, 35.0);
        cx.notify();
    }
}

fn file_name(path: &Path) -> SharedString {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
        .into()
}

fn display_path(path: &Path) -> String {
    let display = path.to_string_lossy();
    if let Some(unc) = display.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        display.strip_prefix(r"\\?\").unwrap_or(&display).to_owned()
    }
}

fn syntax_for(path: &Path) -> String {
    path.extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("text")
        .to_ascii_lowercase()
}

fn recent_warning(error: std::io::Error) -> SharedString {
    format!("Список недавних файлов недоступен. {error}").into()
}
