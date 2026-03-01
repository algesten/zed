mod search_panel_settings;

use anyhow::Context as _;
use collections::{HashSet, IndexMap};
use db::kvp::KEY_VALUE_STORE;
use editor::{Editor, EditorEvent, EditorSettings, SelectionEffects};
use file_icons::FileIcons;
use futures::StreamExt;
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, KeyContext, ParentElement, Render, ScrollStrategy,
    SharedString, Styled, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, actions,
    div, uniform_list,
};
use language::Buffer;
use project::{
    Project, ProjectPath,
    search::{SearchQuery, SearchResult},
};
use search::{
    SearchOptions, ToggleCaseSensitive, ToggleIncludeIgnored, ToggleRegex, ToggleWholeWord,
};
use search_panel_settings::SearchPanelSettings;
use serde::{Deserialize, Serialize};
use settings::{SearchMode, Settings};
use std::{ops::Range, pin::pin};
use text::{ToOffset, ToPoint};
use ui::{prelude::*, Tooltip};
use util::ResultExt;
use workspace::{
    DeploySearch, PreviewTabsSettings, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    searchable::SearchableItemHandle,
};

actions!(
    search_panel,
    [
        ToggleFocus,
        NextEntry,
        PreviousEntry,
        ExpandEntry,
        CollapseEntry,
        OpenEntry,
        ToggleFilters,
        ToggleReplace,
        ReplaceAll,
        AcceptReplacement,
        DismissMatch,
        ClosePanel,
    ]
);

const SEARCH_PANEL_KEY: &str = "SearchPanel";
const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        register(workspace, cx);
    })
    .detach();
}

fn register(workspace: &mut Workspace, _cx: &mut gpui::Context<Workspace>) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<SearchPanel>(window, cx);
    });
    workspace.register_action(|workspace, _: &ClosePanel, window, cx| {
        workspace.close_panel::<SearchPanel>(window, cx);
    });
    workspace.register_action(|workspace, _action: &DeploySearch, window, cx| {
        let search_mode = EditorSettings::get_global(cx).search.search_mode;
        match search_mode {
            SearchMode::Panel => {
                if let Some(panel) = workspace.panel::<SearchPanel>(cx) {
                    workspace.focus_panel::<SearchPanel>(window, cx);
                    panel.update(cx, |panel, cx| {
                        panel.seed_query_from_active_editor(workspace, window, cx);
                    });
                }
            }
            SearchMode::Default => {
                cx.propagate();
            }
        }
    });
}

struct FileMatchData {
    buffer: Entity<Buffer>,
    matches: Vec<MatchData>,
}

struct MatchData {
    range: Range<text::Anchor>,
    line_text: SharedString,
    match_range_in_line: Range<usize>,
}

#[derive(Clone)]
enum SearchPanelEntry {
    FileHeader {
        path: ProjectPath,
        display_path: SharedString,
        match_count: usize,
        collapsed: bool,
    },
    Match {
        path: ProjectPath,
        match_index: usize,
        line_text: SharedString,
        match_range_in_line: Range<usize>,
    },
}

pub struct SearchPanel {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    width: Option<Pixels>,
    scroll_handle: UniformListScrollHandle,

    query_editor: Entity<Editor>,
    replacement_editor: Entity<Editor>,
    include_editor: Entity<Editor>,
    exclude_editor: Entity<Editor>,

    search_options: SearchOptions,
    replace_enabled: bool,
    filters_enabled: bool,
    included_opened_only: bool,
    active_query: Option<SearchQuery>,
    pending_search: Option<Task<Option<()>>>,
    search_debounce: Option<Task<()>>,
    search_id: usize,

    file_matches: IndexMap<ProjectPath, FileMatchData>,
    collapsed_files: HashSet<ProjectPath>,
    entries: Vec<SearchPanelEntry>,
    selected_entry_index: Option<usize>,
    match_count: usize,
    limit_reached: bool,
    no_results: Option<bool>,

    pending_serialization: Task<()>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Serialize, Deserialize)]
struct SerializedSearchPanel {
    width: Option<Pixels>,
}

impl SearchPanel {
    fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut gpui::Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let weak_workspace = workspace.weak_handle();

        let query_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search", window, cx);
            editor
        });
        let replacement_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Replace", window, cx);
            editor
        });
        let include_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Files to include (e.g. *.rs, src/)", window, cx);
            editor
        });
        let exclude_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Files to exclude (e.g. *.log, target/)", window, cx);
            editor
        });

        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let mut subscriptions = Vec::new();

            subscriptions.push(cx.subscribe_in(
                &query_editor,
                window,
                |this: &mut Self, _, event: &EditorEvent, _window, cx| {
                    if matches!(event, EditorEvent::BufferEdited { .. }) {
                        this.query_changed(cx);
                    }
                },
            ));
            subscriptions.push(cx.subscribe_in(
                &include_editor,
                window,
                |this: &mut Self, _, event: &EditorEvent, _window, cx| {
                    if matches!(event, EditorEvent::BufferEdited { .. }) {
                        this.query_changed(cx);
                    }
                },
            ));
            subscriptions.push(cx.subscribe_in(
                &exclude_editor,
                window,
                |this: &mut Self, _, event: &EditorEvent, _window, cx| {
                    if matches!(event, EditorEvent::BufferEdited { .. }) {
                        this.query_changed(cx);
                    }
                },
            ));

            Self {
                project,
                workspace: weak_workspace,
                focus_handle,
                width: None,
                scroll_handle: UniformListScrollHandle::new(),
                query_editor,
                replacement_editor,
                include_editor,
                exclude_editor,
                search_options: SearchOptions::from_settings(
                    &EditorSettings::get_global(cx).search,
                ),
                replace_enabled: false,
                filters_enabled: false,
                included_opened_only: false,
                active_query: None,
                pending_search: None,
                search_debounce: None,
                search_id: 0,
                file_matches: IndexMap::default(),
                collapsed_files: HashSet::default(),
                entries: Vec::new(),
                selected_entry_index: None,
                match_count: 0,
                limit_reached: false,
                no_results: None,
                pending_serialization: Task::ready(()),
                _subscriptions: subscriptions,
            }
        })
    }

    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            let serialized_panel = match workspace
                .read_with(cx, |workspace, _| Self::serialization_key(workspace))
                .ok()
                .flatten()
            {
                Some(serialization_key) => cx
                    .background_spawn(async move { KEY_VALUE_STORE.read_kvp(&serialization_key) })
                    .await
                    .context("loading search panel")
                    .log_err()
                    .flatten()
                    .map(|panel| serde_json::from_str::<SerializedSearchPanel>(&panel))
                    .transpose()
                    .log_err()
                    .flatten(),
                None => None,
            };

            workspace.update_in(cx, |workspace, window, cx| {
                let panel = SearchPanel::new(workspace, window, cx);

                if let Some(serialized_panel) = serialized_panel {
                    panel.update(cx, |panel, cx| {
                        panel.width = serialized_panel.width;
                        cx.notify();
                    });
                }

                panel
            })
        })
    }

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{}-{:?}", SEARCH_PANEL_KEY, id))
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let width = self.width;

        self.pending_serialization = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(workspace::SERIALIZATION_THROTTLE_TIME)
                .await;
            let Some(serialization_key) = this
                .update(cx, |this, cx| {
                    this.workspace
                        .read_with(cx, |workspace, _| Self::serialization_key(workspace))
                        .ok()
                        .flatten()
                })
                .ok()
                .flatten()
            else {
                return;
            };
            cx.background_spawn(async move {
                KEY_VALUE_STORE
                    .write_kvp(
                        serialization_key,
                        serde_json::to_string(&SerializedSearchPanel { width })?,
                    )
                    .await?;
                anyhow::Ok(())
            })
            .await
            .log_err();
        });
    }

    fn dispatch_context(&self, _window: &Window, _cx: &Context<Self>) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults();
        dispatch_context.add("SearchPanel");
        dispatch_context.add("menu");
        dispatch_context
    }

    fn seed_query_from_active_editor(
        &mut self,
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(active_item) = workspace.active_item(cx) {
            if let Some(editor) = active_item.act_as::<Editor>(cx) {
                let query = editor.query_suggestion(window, cx);
                if !query.is_empty() {
                    self.query_editor.update(cx, |editor, cx| {
                        editor.set_text(query, window, cx);
                    });
                }
            }
        }
        self.focus_query_editor(window, cx);
    }

    fn focus_query_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.query_editor.update(cx, |editor, cx| {
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
        let focus_handle = self.query_editor.focus_handle(cx);
        window.focus(&focus_handle, cx);
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        self.search_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                this.execute_search(cx);
            })
            .log_err();
        }));
    }

    fn execute_search(&mut self, cx: &mut Context<Self>) {
        let query_text = self.query_editor.read(cx).text(cx);
        if query_text.is_empty() {
            self.clear_results(cx);
            return;
        }

        let include_text = if self.filters_enabled {
            self.include_editor.read(cx).text(cx)
        } else {
            String::new()
        };
        let exclude_text = if self.filters_enabled {
            self.exclude_editor.read(cx).text(cx)
        } else {
            String::new()
        };

        let path_style = self.project.read(cx).path_style(cx);

        let files_to_include = util::paths::PathMatcher::new(
            &split_glob_patterns(&include_text)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            path_style,
        );
        let files_to_exclude = util::paths::PathMatcher::new(
            &split_glob_patterns(&exclude_text)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            path_style,
        );

        let Some(files_to_include) = files_to_include.log_err() else {
            return;
        };
        let Some(files_to_exclude) = files_to_exclude.log_err() else {
            return;
        };

        let is_regex = self.search_options.contains(SearchOptions::REGEX);
        let whole_word = self.search_options.contains(SearchOptions::WHOLE_WORD);
        let case_sensitive = self.search_options.contains(SearchOptions::CASE_SENSITIVE);
        let include_ignored = self.search_options.contains(SearchOptions::INCLUDE_IGNORED);

        let open_buffers = if self.included_opened_only {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace
                        .items_of_type::<Editor>(cx)
                        .filter_map(|editor| editor.read(cx).buffer().read(cx).as_singleton())
                        .collect()
                })
                .ok()
        } else {
            None
        };

        let query = if is_regex {
            SearchQuery::regex(
                &query_text,
                whole_word,
                case_sensitive,
                include_ignored,
                false,
                files_to_include,
                files_to_exclude,
                false,
                open_buffers,
            )
        } else {
            SearchQuery::text(
                &query_text,
                whole_word,
                case_sensitive,
                include_ignored,
                files_to_include,
                files_to_exclude,
                false,
                open_buffers,
            )
        };

        let query = match query {
            Ok(query) => query,
            Err(_) => return,
        };

        let search = self
            .project
            .update(cx, |project, cx| project.search(query.clone(), cx));

        self.search_id += 1;
        self.active_query = Some(query);
        self.file_matches.clear();
        self.collapsed_files.clear();
        self.match_count = 0;
        self.limit_reached = false;
        self.no_results = Some(true);

        self.pending_search = Some(cx.spawn(async move |this, cx| {
            let project::SearchResults { rx, _task_handle } = search;
            let mut matches = pin!(rx.ready_chunks(1024));

            while let Some(results) = matches.next().await {
                let (buffers_with_ranges, has_reached_limit) = cx
                    .background_executor()
                    .spawn(async move {
                        let mut limit_reached = false;
                        let mut buffers_with_ranges = Vec::with_capacity(results.len());
                        for result in results {
                            match result {
                                SearchResult::Buffer { buffer, ranges } => {
                                    buffers_with_ranges.push((buffer, ranges));
                                }
                                SearchResult::LimitReached => {
                                    limit_reached = true;
                                }
                            }
                        }
                        (buffers_with_ranges, limit_reached)
                    })
                    .await;

                this.update(cx, |this, cx| {
                    for (buffer, ranges) in buffers_with_ranges {
                        if ranges.is_empty() {
                            continue;
                        }
                        let snapshot = buffer.read(cx).snapshot();
                        let file = buffer.read(cx).file();
                        let project_path = match file {
                            Some(file) => ProjectPath {
                                worktree_id: file.worktree_id(cx),
                                path: file.path().clone(),
                            },
                            None => continue,
                        };

                        let mut match_data = Vec::with_capacity(ranges.len());
                        for range in &ranges {
                            let start_point = range.start.to_point(&snapshot);
                            let end_point = range.end.to_point(&snapshot);
                            let line_start =
                                snapshot.point_to_offset(text::Point::new(start_point.row, 0));
                            let line_end_row = if end_point.row == start_point.row {
                                start_point.row
                            } else {
                                end_point.row
                            };
                            let line_end = snapshot.point_to_offset(text::Point::new(
                                line_end_row,
                                snapshot.line_len(line_end_row),
                            ));
                            let line_text: SharedString = snapshot
                                .text_for_range(line_start..line_end)
                                .collect::<String>()
                                .into();

                            let match_start_in_line =
                                range.start.to_offset(&snapshot) - line_start;
                            let match_end_in_line = if end_point.row == start_point.row {
                                range.end.to_offset(&snapshot) - line_start
                            } else {
                                line_text.len()
                            };

                            match_data.push(MatchData {
                                range: range.clone(),
                                line_text: line_text.clone(),
                                match_range_in_line: match_start_in_line..match_end_in_line,
                            });
                        }

                        this.match_count += match_data.len();

                        let entry =
                            this.file_matches.entry(project_path).or_insert_with(|| {
                                FileMatchData {
                                    buffer: buffer.clone(),
                                    matches: Vec::new(),
                                }
                            });
                        entry.matches.extend(match_data);
                    }

                    if has_reached_limit {
                        this.limit_reached = true;
                    }

                    if this.match_count > 0 {
                        this.no_results = Some(false);
                    }

                    this.rebuild_entries(cx);
                    cx.notify();
                })
                .ok()?;
            }

            this.update(cx, |this, cx| {
                this.pending_search.take();
                cx.notify();
            })
            .ok()?;

            None
        }));

        self.rebuild_entries(cx);
        cx.notify();
    }

    fn clear_results(&mut self, cx: &mut Context<Self>) {
        self.file_matches.clear();
        self.collapsed_files.clear();
        self.entries.clear();
        self.selected_entry_index = None;
        self.match_count = 0;
        self.limit_reached = false;
        self.no_results = None;
        self.active_query = None;
        self.pending_search = None;
        cx.notify();
    }

    fn rebuild_entries(&mut self, cx: &App) {
        let path_style = self.project.read(cx).path_style(cx);
        self.entries.clear();
        for (path, file_match) in &self.file_matches {
            let collapsed = self.collapsed_files.contains(path);
            let display_path: SharedString = path.path.display(path_style).to_string().into();

            self.entries.push(SearchPanelEntry::FileHeader {
                path: path.clone(),
                display_path,
                match_count: file_match.matches.len(),
                collapsed,
            });

            if !collapsed {
                for (match_index, match_data) in file_match.matches.iter().enumerate() {
                    self.entries.push(SearchPanelEntry::Match {
                        path: path.clone(),
                        match_index,
                        line_text: match_data.line_text.clone(),
                        match_range_in_line: match_data.match_range_in_line.clone(),
                    });
                }
            }
        }
    }

    fn toggle_search_option(&mut self, option: SearchOptions, cx: &mut Context<Self>) {
        self.search_options.toggle(option);
        cx.notify();
        self.execute_search(cx);
    }

    fn toggle_replace(
        &mut self,
        _: &ToggleReplace,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_enabled = !self.replace_enabled;
        cx.notify();
    }

    fn toggle_filters(
        &mut self,
        _: &ToggleFilters,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.filters_enabled = !self.filters_enabled;
        cx.notify();
    }

    fn toggle_case_sensitive(
        &mut self,
        _: &ToggleCaseSensitive,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_search_option(SearchOptions::CASE_SENSITIVE, cx);
    }

    fn toggle_whole_word(
        &mut self,
        _: &ToggleWholeWord,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_search_option(SearchOptions::WHOLE_WORD, cx);
    }

    fn toggle_regex(
        &mut self,
        _: &ToggleRegex,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_search_option(SearchOptions::REGEX, cx);
    }

    fn toggle_include_ignored(
        &mut self,
        _: &ToggleIncludeIgnored,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_search_option(SearchOptions::INCLUDE_IGNORED, cx);
    }

    fn select_next(&mut self, _: &NextEntry, _window: &mut Window, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        let next = match self.selected_entry_index {
            Some(ix) if ix + 1 < self.entries.len() => ix + 1,
            Some(_) => 0,
            None => 0,
        };
        self.selected_entry_index = Some(next);
        self.scroll_handle
            .scroll_to_item(next, ScrollStrategy::Center);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &PreviousEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.entries.is_empty() {
            return;
        }
        let prev = match self.selected_entry_index {
            Some(0) => self.entries.len() - 1,
            Some(ix) => ix - 1,
            None => self.entries.len() - 1,
        };
        self.selected_entry_index = Some(prev);
        self.scroll_handle
            .scroll_to_item(prev, ScrollStrategy::Center);
        cx.notify();
    }

    fn expand_entry(
        &mut self,
        _: &ExpandEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.selected_entry_index {
            if let Some(SearchPanelEntry::FileHeader { path, .. }) = self.entries.get(ix) {
                self.collapsed_files.remove(path);
                self.rebuild_entries(cx);
                cx.notify();
            }
        }
    }

    fn collapse_entry(
        &mut self,
        _: &CollapseEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.selected_entry_index {
            if let Some(SearchPanelEntry::FileHeader { path, .. }) = self.entries.get(ix) {
                self.collapsed_files.insert(path.clone());
                self.rebuild_entries(cx);
                cx.notify();
            }
        }
    }

    fn open_entry(&mut self, _: &OpenEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.open_selected_match(false, window, cx);
    }

    fn open_selected_match(
        &mut self,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selected_entry_index else {
            return;
        };
        let entry = match self.entries.get(ix) {
            Some(entry) => entry.clone(),
            None => return,
        };

        match entry {
            SearchPanelEntry::Match {
                path,
                match_index,
                ..
            } => {
                if let Some(file_match) = self.file_matches.get(&path) {
                    if let Some(match_data) = file_match.matches.get(match_index) {
                        let range = match_data.range.clone();
                        let buffer = file_match.buffer.clone();

                        if let Some(workspace) = self.workspace.upgrade() {
                            workspace.update(cx, |workspace, cx| {
                                let task = workspace.open_path_preview(
                                    path, None, true, preview, true, window, cx,
                                );
                                cx.spawn_in(window, async move |_, cx| {
                                    let item = task.await?;
                                    let editor = cx
                                        .update(|_, cx| item.act_as::<Editor>(cx))?
                                        .context("expected editor")?;
                                    editor.update_in(cx, |editor, window, cx| {
                                        let snapshot = buffer.read(cx).snapshot();
                                        let start = range.start.to_point(&snapshot);
                                        let end = range.end.to_point(&snapshot);
                                        editor.change_selections(
                                            SelectionEffects::scroll(
                                                editor::scroll::Autoscroll::center(),
                                            ),
                                            window,
                                            cx,
                                            |selections| {
                                                selections.select_ranges([start..end]);
                                            },
                                        );
                                    })?;
                                    anyhow::Ok(())
                                })
                                .detach_and_log_err(cx);
                            });
                        }
                    }
                }
            }
            SearchPanelEntry::FileHeader {
                path, collapsed, ..
            } => {
                if collapsed {
                    self.collapsed_files.remove(&path);
                } else {
                    self.collapsed_files.insert(path);
                }
                self.rebuild_entries(cx);
                cx.notify();
            }
        }
    }

    fn replace_all(&mut self, _: &ReplaceAll, _window: &mut Window, cx: &mut Context<Self>) {
        let replacement_text = self.replacement_editor.read(cx).text(cx);
        let Some(query) = &self.active_query else {
            return;
        };

        let paths: Vec<_> = self.file_matches.keys().cloned().collect();
        for path in paths {
            if let Some(file_match) = self.file_matches.get(&path) {
                let buffer = file_match.buffer.clone();
                let edits: Vec<_> = file_match
                    .matches
                    .iter()
                    .rev()
                    .map(|m| {
                        let matched_text =
                            m.line_text.get(m.match_range_in_line.clone()).unwrap_or("");
                        let replacement = query
                            .replacement_for(matched_text)
                            .map(|cow| cow.into_owned())
                            .unwrap_or_else(|| replacement_text.clone());
                        (m.range.clone(), replacement)
                    })
                    .collect();

                buffer.update(cx, |buffer, cx| {
                    buffer.edit(edits, None, cx);
                });
            }
        }

        self.clear_results(cx);
    }

    fn accept_replacement(
        &mut self,
        _: &AcceptReplacement,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let replacement_text = self.replacement_editor.read(cx).text(cx);
        let Some(ix) = self.selected_entry_index else {
            return;
        };

        let entry = match self.entries.get(ix) {
            Some(entry) => entry.clone(),
            None => return,
        };

        if let SearchPanelEntry::Match {
            path, match_index, ..
        } = entry
        {
            if let Some(file_match) = self.file_matches.get(&path) {
                if let Some(match_data) = file_match.matches.get(match_index) {
                    let matched_text = match_data
                        .line_text
                        .get(match_data.match_range_in_line.clone())
                        .unwrap_or("");
                    let query = self.active_query.as_ref();
                    let replacement = query
                        .and_then(|q| q.replacement_for(matched_text))
                        .map(|cow| cow.into_owned())
                        .unwrap_or_else(|| replacement_text.clone());

                    let range = match_data.range.clone();
                    let buffer = file_match.buffer.clone();
                    buffer.update(cx, |buffer, cx| {
                        buffer.edit([(range, replacement)], None, cx);
                    });
                }
            }

            self.remove_match(&path, match_index);
            self.rebuild_entries(cx);
            cx.notify();
        }
    }

    fn dismiss_match(
        &mut self,
        _: &DismissMatch,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selected_entry_index else {
            return;
        };

        let entry = match self.entries.get(ix) {
            Some(entry) => entry.clone(),
            None => return,
        };

        if let SearchPanelEntry::Match {
            path, match_index, ..
        } = entry
        {
            self.remove_match(&path, match_index);
            self.rebuild_entries(cx);
            cx.notify();
        }
    }

    fn remove_match(&mut self, path: &ProjectPath, match_index: usize) {
        if let Some(file_match) = self.file_matches.get_mut(path) {
            if match_index < file_match.matches.len() {
                file_match.matches.remove(match_index);
                self.match_count = self.match_count.saturating_sub(1);
            }
            if file_match.matches.is_empty() {
                self.file_matches.swap_remove(path);
            }
        }
    }

    fn render_query_input(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let case_sensitive = self.search_options.contains(SearchOptions::CASE_SENSITIVE);
        let whole_word = self.search_options.contains(SearchOptions::WHOLE_WORD);
        let regex = self.search_options.contains(SearchOptions::REGEX);

        let editor_container = |child: Entity<Editor>| {
            div()
                .flex_1()
                .px_2()
                .py_0p5()
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .rounded_md()
                .bg(cx.theme().colors().editor_background)
                .child(child)
        };

        let query_input = h_flex()
            .flex_1()
            .pl_2()
            .pr_1()
            .py_0p5()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_md()
            .bg(cx.theme().colors().editor_background)
            .child(div().flex_1().child(self.query_editor.clone()))
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("toggle-case-sensitive", ui::IconName::CaseSensitive)
                            .shape(ui::IconButtonShape::Square)
                            .size(ButtonSize::Compact)
                            .toggle_state(case_sensitive)
                            .tooltip(Tooltip::text("Match Case"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.toggle_search_option(SearchOptions::CASE_SENSITIVE, cx);
                            })),
                    )
                    .child(
                        IconButton::new("toggle-whole-word", ui::IconName::WholeWord)
                            .shape(ui::IconButtonShape::Square)
                            .size(ButtonSize::Compact)
                            .toggle_state(whole_word)
                            .tooltip(Tooltip::text("Match Whole Word"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.toggle_search_option(SearchOptions::WHOLE_WORD, cx);
                            })),
                    )
                    .child(
                        IconButton::new("toggle-regex", ui::IconName::Regex)
                            .shape(ui::IconButtonShape::Square)
                            .size(ButtonSize::Compact)
                            .toggle_state(regex)
                            .tooltip(Tooltip::text("Use Regular Expression"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.toggle_search_option(SearchOptions::REGEX, cx);
                            })),
                    ),
            );

        v_flex()
            .gap_1()
            .p_2()
            .child(query_input)
            .child(
                h_flex()
                    .child(
                        h_flex()
                            .flex_1()
                            .child(
                                IconButton::new("toggle-replace", ui::IconName::Replace)
                                    .shape(ui::IconButtonShape::Square)
                                    .size(ButtonSize::Compact)
                                    .toggle_state(self.replace_enabled)
                                    .tooltip(Tooltip::text("Toggle Replace"))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.replace_enabled = !this.replace_enabled;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                IconButton::new(
                                    "search-opened-only",
                                    ui::IconName::FolderSearch,
                                )
                                .shape(ui::IconButtonShape::Square)
                                .size(ButtonSize::Compact)
                                .toggle_state(self.included_opened_only)
                                .tooltip(Tooltip::text("Only Search Open Files"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.included_opened_only = !this.included_opened_only;
                                    cx.notify();
                                    this.execute_search(cx);
                                })),
                            )
                            .child(
                                IconButton::new(
                                    "toggle-include-ignored",
                                    ui::IconName::Sliders,
                                )
                                .shape(ui::IconButtonShape::Square)
                                .size(ButtonSize::Compact)
                                .toggle_state(
                                    self.search_options
                                        .contains(SearchOptions::INCLUDE_IGNORED),
                                )
                                .tooltip(Tooltip::text(
                                    "Also search files ignored by configuration",
                                ))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.toggle_search_option(
                                        SearchOptions::INCLUDE_IGNORED,
                                        cx,
                                    );
                                })),
                            )
                            .child(
                                IconButton::new("toggle-filters", ui::IconName::Filter)
                                    .shape(ui::IconButtonShape::Square)
                                    .size(ButtonSize::Compact)
                                    .toggle_state(self.filters_enabled)
                                    .tooltip(Tooltip::text("Toggle Filters"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_filters(&ToggleFilters, window, cx);
                                    })),
                            ),
                    ),
            )
            .when(self.replace_enabled, |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(editor_container(self.replacement_editor.clone()))
                        .child(
                            IconButton::new("replace-all", ui::IconName::ReplaceAll)
                                .shape(ui::IconButtonShape::Square)
                                .size(ButtonSize::Compact)
                                .tooltip(Tooltip::text("Replace All Matches"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.replace_all(&ReplaceAll, window, cx);
                                })),
                        ),
                )
            })
            .when(self.filters_enabled, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(editor_container(self.include_editor.clone()))
                        .child(editor_container(self.exclude_editor.clone())),
                )
            })
    }

    fn render_results(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entry_count = self.entries.len();

        if entry_count == 0 {
            return div()
                .flex_1()
                .size_full()
                .child(self.render_empty_state(cx))
                .into_any_element();
        }

        div()
            .flex_1()
            .size_full()
            .child(
                uniform_list(
                    "search-results",
                    entry_count,
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        let mut items = Vec::with_capacity(range.end - range.start);
                        for ix in range {
                            if let Some(entry) = this.entries.get(ix) {
                                items.push(this.render_entry(ix, entry.clone(), cx));
                            }
                        }
                        items
                    }),
                )
                .size_full()
                .track_scroll(&self.scroll_handle),
            )
            .when(self.limit_reached, |this| {
                this.child(div().px_2().py_1().child(
                    Label::new(
                        "Search limit reached. Narrow your search for complete results.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Warning),
                ))
            })
            .into_any_element()
    }

    fn render_empty_state(&self, _cx: &Context<Self>) -> impl IntoElement {
        let message = if self.pending_search.is_some() {
            "Searching..."
        } else if let Some(true) = self.no_results {
            "No results found"
        } else {
            "Search to find results"
        };

        div()
            .flex_1()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(Label::new(message).size(LabelSize::Small).color(Color::Muted))
    }

    fn render_entry(
        &self,
        ix: usize,
        entry: SearchPanelEntry,
        cx: &Context<Self>,
    ) -> AnyElement {
        let is_selected = self.selected_entry_index == Some(ix);

        match entry {
            SearchPanelEntry::FileHeader {
                path,
                display_path,
                match_count,
                collapsed,
            } => {
                let file_name = path
                    .path
                    .file_name()
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| display_path.to_string());

                let icon = FileIcons::get_icon(path.path.as_std_path(), cx);

                div()
                    .id(("file-header", ix))
                    .w_full()
                    .px_2()
                    .py_0p5()
                    .flex()
                    .items_center()
                    .gap_1()
                    .when(is_selected, |this| {
                        this.bg(cx.theme().colors().ghost_element_selected)
                    })
                    .hover(|this| this.bg(cx.theme().colors().ghost_element_hover))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .when_some(icon, |this, icon| {
                        this.child(Icon::from_path(icon).size(IconSize::Small))
                    })
                    .child(Label::new(file_name).size(LabelSize::Small).single_line())
                    .child(
                        Label::new(display_path.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line()
                            .ml_1(),
                    )
                    .child(
                        Label::new(format!("{match_count}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .ml_auto(),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected_entry_index = Some(ix);
                        this.open_selected_match(true, window, cx);
                    }))
                    .into_any_element()
            }
            SearchPanelEntry::Match {
                line_text,
                match_range_in_line,
                ..
            } => {
                const MAX_CONTEXT_CHARS: usize = 20;
                let before_match_full = line_text
                    .get(..match_range_in_line.start)
                    .unwrap_or("")
                    .trim_start();
                let before_match = if before_match_full.len() > MAX_CONTEXT_CHARS {
                    let start = before_match_full.len() - MAX_CONTEXT_CHARS;
                    let start = before_match_full
                        .ceil_char_boundary(start);
                    format!("…{}", &before_match_full[start..])
                } else {
                    before_match_full.to_string()
                };
                let matched_text =
                    line_text.get(match_range_in_line.clone()).unwrap_or("");
                let after_match = line_text.get(match_range_in_line.end..).unwrap_or("");
                let replace_enabled = self.replace_enabled;
                let replacement_text = if replace_enabled {
                    Some(self.replacement_editor.read(cx).text(cx))
                } else {
                    None
                };

                div()
                    .id(("match", ix))
                    .w_full()
                    .px_2()
                    .pl_6()
                    .py_0p5()
                    .flex()
                    .items_center()
                    .gap_1()
                    .when(is_selected, |this| {
                        this.bg(cx.theme().colors().ghost_element_selected)
                    })
                    .hover(|this| this.bg(cx.theme().colors().ghost_element_hover))
                    .child(
                        h_flex()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                Label::new(before_match)
                                    .size(LabelSize::Small)
                                    .single_line(),
                            )
                            .when(replacement_text.is_some(), |this| {
                                let replacement = replacement_text.as_deref().unwrap_or("");
                                this.child(
                                    div()
                                        .rounded_sm()
                                        .bg(cx.theme().colors().version_control_word_deleted)
                                        .child(
                                            Label::new(matched_text.to_string())
                                                .size(LabelSize::Small)
                                                .single_line()
                                                .strikethrough(),
                                        ),
                                )
                                .child(
                                    div()
                                        .rounded_sm()
                                        .bg(cx.theme().colors().version_control_word_added)
                                        .child(
                                            Label::new(replacement.to_string())
                                                .size(LabelSize::Small)
                                                .single_line(),
                                        ),
                                )
                            })
                            .when(replacement_text.is_none(), |this| {
                                this.child(
                                    Label::new(matched_text.to_string())
                                        .size(LabelSize::Small)
                                        .single_line()
                                        .color(Color::Accent),
                                )
                            })
                            .child(
                                Label::new(after_match.to_string())
                                    .size(LabelSize::Small)
                                    .single_line(),
                            ),
                    )
                    .when(replace_enabled, |this| {
                        this.child(
                            h_flex()
                                .gap_0p5()
                                .child(
                                    IconButton::new(("accept", ix), IconName::Check)
                                        .size(ButtonSize::Compact)
                                        .tooltip(Tooltip::text("Accept replacement"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.selected_entry_index = Some(ix);
                                            this.accept_replacement(
                                                &AcceptReplacement,
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    IconButton::new(("dismiss", ix), IconName::Close)
                                        .size(ButtonSize::Compact)
                                        .tooltip(Tooltip::text("Dismiss match"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.selected_entry_index = Some(ix);
                                            this.dismiss_match(&DismissMatch, window, cx);
                                        })),
                                ),
                        )
                    })
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.selected_entry_index = Some(ix);
                        let preview = PreviewTabsSettings::get_global(cx).enabled
                            && event.click_count() == 1;
                        this.open_selected_match(preview, window, cx);
                    }))
                    .into_any_element()
            }
        }
    }

    fn render_summary(&self, _cx: &Context<Self>) -> impl IntoElement {
        let text = if self.match_count > 0 {
            let file_count = self.file_matches.len();
            format!(
                "{} result{} in {} file{}",
                self.match_count,
                if self.match_count == 1 { "" } else { "s" },
                file_count,
                if file_count == 1 { "" } else { "s" },
            )
        } else {
            String::new()
        };

        div().px_2().py_1().when(!text.is_empty(), |this| {
            this.child(Label::new(text).size(LabelSize::XSmall).color(Color::Muted))
        })
    }
}

fn split_glob_patterns(text: &str) -> Vec<&str> {
    let mut patterns = Vec::new();
    let mut pattern_start = 0;
    let mut brace_depth: usize = 0;
    let mut escaped = false;

    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            ',' if brace_depth == 0 => {
                let pattern = text[pattern_start..index].trim();
                if !pattern.is_empty() {
                    patterns.push(pattern);
                }
                pattern_start = index + 1;
            }
            _ => {}
        }
    }

    let last_pattern = text[pattern_start..].trim();
    if !last_pattern.is_empty() {
        patterns.push(last_pattern);
    }

    patterns
}

impl Render for SearchPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("search_panel")
            .key_context(self.dispatch_context(window, cx))
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_case_sensitive))
            .on_action(cx.listener(Self::toggle_whole_word))
            .on_action(cx.listener(Self::toggle_regex))
            .on_action(cx.listener(Self::toggle_include_ignored))
            .on_action(cx.listener(Self::toggle_replace))
            .on_action(cx.listener(Self::toggle_filters))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::expand_entry))
            .on_action(cx.listener(Self::collapse_entry))
            .on_action(cx.listener(Self::open_entry))
            .on_action(cx.listener(Self::replace_all))
            .on_action(cx.listener(Self::accept_replacement))
            .on_action(cx.listener(Self::dismiss_match))
            .size_full()
            .overflow_hidden()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_query_input(window, cx))
            .child(self.render_summary(cx))
            .child(self.render_results(window, cx))
    }
}

impl Focusable for SearchPanel {
    fn focus_handle(&self, _cx: &App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for SearchPanel {}

impl Panel for SearchPanel {
    fn persistent_name() -> &'static str {
        "SearchPanel"
    }

    fn panel_key() -> &'static str {
        SEARCH_PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        SearchPanelSettings::get_global(cx).dock
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(
            self.project.read(cx).fs().clone(),
            cx,
            move |settings, _| {
                settings
                    .search_panel
                    .get_or_insert_default()
                    .dock = Some(position.into());
            },
        );
    }

    fn size(&self, _: &Window, cx: &App) -> Pixels {
        self.width
            .unwrap_or_else(|| SearchPanelSettings::get_global(cx).default_width)
    }

    fn set_size(&mut self, size: Option<Pixels>, _: &mut Window, cx: &mut Context<Self>) {
        self.width = size;
        self.serialize(cx);
        cx.notify();
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::MagnifyingGlass).filter(|_| SearchPanelSettings::get_global(cx).button)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Search Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        3
    }
}

impl panel::PanelHeader for SearchPanel {}
