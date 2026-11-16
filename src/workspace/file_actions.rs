use std::path::{Path, PathBuf};

use gpui_kit::*;

use super::{Workspace, disk_status::DiskStatus, file_name, recent_warning, syntax_for};
use crate::{
    files::{self, FileTarget, OpenedFile},
    log_file::LogBookmark,
    recent_files::RecentFile,
    text_format::TextEncoding,
};

#[derive(Clone, Copy)]
pub(super) struct OpenMode {
    windows_1251: bool,
    view_as_log: bool,
}

impl OpenMode {
    pub(super) const NORMAL: Self = Self {
        windows_1251: false,
        view_as_log: false,
    };
    pub(super) const WINDOWS_1251: Self = Self {
        windows_1251: true,
        view_as_log: false,
    };
    pub(super) const LOG: Self = Self {
        windows_1251: false,
        view_as_log: true,
    };
    pub(super) const LOG_1251: Self = Self {
        windows_1251: true,
        view_as_log: true,
    };

    pub(super) fn from_recent(file: &RecentFile) -> Self {
        Self {
            windows_1251: file.windows_1251,
            view_as_log: file.view_as_log,
        }
    }
}

enum LoadedDocument {
    Text(OpenedFile),
    Log(FileTarget),
}

fn read_document(path: &Path, mode: OpenMode) -> std::io::Result<LoadedDocument> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Выберите обычный файл.",
        ));
    }
    if mode.view_as_log || metadata.len() > files::MAX_EDITOR_BYTES {
        Ok(LoadedDocument::Log(FileTarget::from_picker(
            path.to_path_buf(),
        )?))
    } else {
        files::open(path, mode.windows_1251).map(LoadedDocument::Text)
    }
}

impl Workspace {
    pub(super) fn open_picker(
        &mut self,
        mode: OpenMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("Выбор файла…");
        self.error = None;
        let result = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Открыть".into()),
        });
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = result.await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(Ok(Some(paths))) if !paths.is_empty() => {
                        this.open_paths(paths, mode, window, cx);
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        this.error = Some(format!("Не удалось открыть диалог. {error}").into())
                    }
                    Err(_) => this.error = Some("Диалог открытия прерван".into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn open_paths(
        &mut self,
        paths: Vec<PathBuf>,
        mode: OpenMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.load_paths(paths, mode, None, window, cx);
    }

    pub(super) fn open_closed_file(
        &mut self,
        id: u64,
        recent: RecentFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mode = OpenMode::from_recent(&recent);
        self.load_paths(vec![recent.path], mode, Some(id), window, cx);
    }

    fn load_paths(
        &mut self,
        paths: Vec<PathBuf>,
        mode: OpenMode,
        closed_id: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() || self.busy.is_some() {
            return;
        }

        self.busy = Some("Открытие…");
        self.error = None;
        let executor = cx.background_executor().clone();
        let mut recent = self.recent.clone();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let mut opened_files = Vec::new();
            let mut first_error = None;
            let mut failures = 0;
            for path in paths {
                let name = file_name(&path);
                match executor
                    .spawn(async move { read_document(&path, mode) })
                    .await
                {
                    Ok(opened) => match this.update_in(cx, |this, window, cx| match opened {
                        LoadedDocument::Text(document) => this.show_opened(document, window, cx),
                        LoadedDocument::Log(target) => {
                            this.show_log(target, mode.windows_1251, window, cx)
                        }
                    }) {
                        Ok(file) => opened_files.push(file),
                        Err(_) => return,
                    },
                    Err(error) => {
                        failures += 1;
                        first_error
                            .get_or_insert_with(|| format!("Не удалось открыть «{name}». {error}"));
                    }
                }
            }

            let opened_any = !opened_files.is_empty();
            let (recent, history_error) = executor
                .spawn(async move {
                    let error = recent.record(opened_files).err();
                    (recent, error)
                })
                .await;
            this.update_in(cx, |this, _, cx| {
                if let Some(id) = closed_id
                    && opened_any
                {
                    this.closed.retain(|entry| entry.tab.id != id);
                }
                this.busy = None;
                this.recent = recent;
                this.error = first_error
                    .map(|error| {
                        if failures == 1 {
                            error.into()
                        } else {
                            format!("{error} Ошибок при открытии: {failures}").into()
                        }
                    })
                    .or_else(|| history_error.map(recent_warning));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn show_opened(
        &mut self,
        opened: OpenedFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RecentFile {
        if let Some(doc) = self.documents.iter_mut().find(|doc| {
            doc.log.is_none()
                && doc
                    .file
                    .as_ref()
                    .is_some_and(|file| file.path == opened.target.path)
        }) {
            let id = doc.id;
            if !doc.dirty && (doc.placeholder || doc.file.as_ref() != Some(&opened.target)) {
                doc.editor
                    .update(cx, |editor, cx| editor.set_value(opened.text, window, cx));
                doc.saved_text = Some(doc.editor.read(cx).text().clone());
                doc.file = Some(opened.target.clone());
                doc.format = opened.format;
                doc.placeholder = false;
                doc.restored_scroll = None;
                doc.disk_status = DiskStatus::Current;
                doc.revision = doc.revision.wrapping_add(1);
            }
            let recent = RecentFile::new(opened.target.path, doc.format.encoding);
            self.select(id, window, cx);
            return recent;
        }

        if self.documents.len() == 1
            && self.active_document().file.is_none()
            && !self.active_document().dirty
        {
            self.documents.clear();
        }
        let recent = RecentFile::new(opened.target.path.clone(), opened.format.encoding);
        self.add_document(Some(opened), window, cx);
        if crate::log_viewer::measurement_start().is_some() {
            window.on_next_frame(|_, _| {
                crate::log_viewer::record_document_ready();
            });
        }
        recent
    }

    fn show_log(
        &mut self,
        target: FileTarget,
        windows_1251: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RecentFile {
        let mut recent = RecentFile::new(
            target.path.clone(),
            if windows_1251 {
                TextEncoding::Windows1251
            } else {
                TextEncoding::Utf8
            },
        );
        recent.view_as_log = true;
        if let Some(doc) = self.documents.iter().find(|doc| {
            doc.log.is_some()
                && doc
                    .file
                    .as_ref()
                    .is_some_and(|file| file.path == target.path)
                && matches!(doc.format.encoding, TextEncoding::Windows1251) == windows_1251
        }) {
            self.select(doc.id, window, cx);
            return recent;
        }
        if self.documents.len() == 1
            && self.active_document().file.is_none()
            && !self.active_document().dirty
        {
            self.documents.clear();
        }
        let id = self.insert_log(target, windows_1251, LogBookmark::default(), window, cx);
        self.select(id, window, cx);
        recent
    }

    pub(super) fn save(&mut self, save_as: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.error = None;
        let doc = self.active_document();
        if doc.log.is_some() {
            return;
        }
        if doc.placeholder {
            self.error = Some("Откройте исходный файл, когда он станет доступен.".into());
            cx.notify();
            return;
        }
        let id = doc.id;

        if !save_as && let Some(file) = doc.file.clone() {
            self.write_file(id, file, window, cx);
            return;
        }

        let directory = doc
            .file
            .as_ref()
            .and_then(|file| file.path.parent())
            .unwrap_or_else(|| Path::new(""));
        let suggested = if doc.file.is_some() {
            doc.name.as_ref()
        } else {
            "Без имени.txt"
        };
        let result = cx.prompt_for_new_path(directory, Some(suggested));
        self.busy = Some("Выбор имени файла…");
        let executor = cx.background_executor().clone();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let target = match result.await {
                Ok(Ok(Some(path))) => executor
                    .spawn(async move { FileTarget::from_picker(path) })
                    .await
                    .map(Some)
                    .map_err(|error| error.to_string()),
                Ok(Ok(None)) => Ok(None),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err("Диалог сохранения прерван".into()),
            };
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match target {
                    Ok(Some(target)) => this.write_file(id, target, window, cx),
                    Ok(None) => {}
                    Err(error) => {
                        this.error = Some(format!("Не удалось выбрать файл. {error}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn write_file(
        &mut self,
        id: u64,
        mut target: FileTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(other) = self.documents.iter().find(|doc| {
            doc.id != id
                && doc
                    .file
                    .as_ref()
                    .is_some_and(|file| file.path == target.path)
        }) {
            self.error = Some(format!("Файл «{}» уже открыт в другой вкладке", other.name).into());
            cx.notify();
            return;
        }

        let Some(doc) = self.documents.iter().find(|doc| doc.id == id) else {
            return;
        };
        if let Some(file) = &doc.file
            && file.path == target.path
        {
            target.version = file.version;
        }

        let snapshot = doc.editor.read(cx).text().clone();
        let format = doc.format;
        let mut recent = self.recent.clone();
        self.busy = Some("Сохранение…");
        cx.notify();
        let task = cx.background_spawn(async move {
            let result = files::save(target, format, &snapshot.to_string());
            let history_error = result.as_ref().ok().and_then(|file| {
                recent
                    .record([RecentFile::new(file.path.clone(), format.encoding)])
                    .err()
            });
            (snapshot, result, recent, history_error)
        });
        cx.spawn_in(window, async move |this, cx| {
            let (snapshot, result, recent, history_error) = task.await;
            this.update_in(cx, |this, _, cx| {
                this.busy = None;
                this.recent = recent;
                this.error = history_error.map(recent_warning);
                match result {
                    Ok(file) => {
                        if let Some(doc) = this.documents.iter_mut().find(|doc| doc.id == id) {
                            let language = syntax_for(&file.path);
                            doc.editor
                                .update(cx, |editor, cx| editor.set_highlighter(language, cx));
                            doc.name = file_name(&file.path);
                            doc.file = Some(file);
                            doc.saved_text = Some(snapshot);
                            doc.disk_status = DiskStatus::Current;
                            doc.dirty = doc.saved_text.as_ref() != Some(doc.editor.read(cx).text());
                        }
                    }
                    Err(error) => {
                        this.error = Some(format!("Не удалось сохранить файл. {error}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}
