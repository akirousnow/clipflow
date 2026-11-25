use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, KeyBinding, ObjectFit, ScrollStrategy,
    SharedString, Subscription, Task, UniformListScrollHandle, Window, actions, div, img,
    prelude::*, px, uniform_list,
};

use crate::core::engine::ClipboardEngine;
use crate::core::model::{ClipKind, ClipSummary, PageCursor, PageRequest, PasteMode, ViewTab};
use crate::core::store::now_epoch_ms;
use crate::platform::auto_paste::trigger_auto_paste;
use crate::platform::updater::{UpdateInfo, perform_self_update};
use crate::ui::text_input::{FilterChanged, FilterInput, bind_filter_input_keys};
use crate::ui::theme::Theme;

pub type SharedUpdateState = Arc<Mutex<Option<UpdateInfo>>>;

/// Number of entries fetched per SQLite page (initial open, scroll-more, and search).
pub const PAGE_SIZE: usize = 40;
/// Prefetch threshold: load the next page when the visible viewport is within this many rows of the end.
const PREFETCH_THRESHOLD: usize = 10;
/// Search input debounce interval before querying SQLite.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(55);
/// Delay after closing the floating window before emitting synthetic Ctrl+V so focus settles cleanly.
const AUTO_PASTE_SETTLE_DELAY: Duration = Duration::from_millis(85);

actions!(
    clipboard_window,
    [
        MoveUp,
        MoveDown,
        ActivatePaste,
        ActivateCopyOnly,
        ActivatePasteShift,
        ToggleFavorite,
        DeleteSelected,
        SwitchTab,
        Dismiss,
    ]
);

pub const WINDOW_CONTEXT: &str = "ClipFlowWindow";

pub fn bind_window_keys(cx: &mut App) {
    bind_filter_input_keys(cx);
    cx.bind_keys([
        KeyBinding::new("up", MoveUp, Some(WINDOW_CONTEXT)),
        KeyBinding::new("ctrl-p", MoveUp, Some(WINDOW_CONTEXT)),
        KeyBinding::new("ctrl-k", MoveUp, Some(WINDOW_CONTEXT)),
        KeyBinding::new("down", MoveDown, Some(WINDOW_CONTEXT)),
        KeyBinding::new("ctrl-n", MoveDown, Some(WINDOW_CONTEXT)),
        KeyBinding::new("ctrl-j", MoveDown, Some(WINDOW_CONTEXT)),
        KeyBinding::new("enter", ActivatePaste, Some(WINDOW_CONTEXT)),
        KeyBinding::new("shift-enter", ActivateCopyOnly, Some(WINDOW_CONTEXT)),
        KeyBinding::new("ctrl-enter", ActivatePasteShift, Some(WINDOW_CONTEXT)),
        KeyBinding::new("ctrl-s", ToggleFavorite, Some(WINDOW_CONTEXT)),
        KeyBinding::new("ctrl-d", DeleteSelected, Some(WINDOW_CONTEXT)),
        KeyBinding::new("tab", SwitchTab, Some(WINDOW_CONTEXT)),
        KeyBinding::new("escape", Dismiss, Some(WINDOW_CONTEXT)),
    ]);
}

pub struct ClipboardWindow {
    engine: Arc<ClipboardEngine>,
    available_update: SharedUpdateState,
    update_status: Option<String>,
    focus_handle: FocusHandle,
    filter_input: Entity<FilterInput>,
    scroll_handle: UniformListScrollHandle,
    theme: Theme,
    active_tab: ViewTab,
    query: String,
    items: Vec<ClipSummary>,
    next_cursor: Option<PageCursor>,
    has_more: bool,
    total_count: usize,
    all_count: usize,
    fav_count: usize,
    selected_index: usize,
    is_loading_initial: bool,
    is_loading_more: bool,
    generation: u64,
    has_been_active: bool,
    dismiss_on_blur: bool,
    debounce_task: Option<Task<()>>,
    load_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ClipboardWindow {
    pub fn new(
        engine: Arc<ClipboardEngine>,
        available_update: SharedUpdateState,
        dismiss_on_blur: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let theme = Theme::from_appearance(window.appearance());
        let focus_handle = cx.focus_handle();
        let filter_input = cx.new(|cx| FilterInput::new("Search clipboard…", theme, cx));

        // Focus the search input immediately on open so typing filters right away.
        let input_focus = filter_input.read(cx).focus_handle(cx);
        input_focus.focus(window);

        let mut subscriptions = Vec::new();

        subscriptions.push(cx.subscribe(&filter_input, |this, _, event: &FilterChanged, cx| {
            this.on_filter_changed(event.0.clone(), cx);
        }));

        subscriptions.push(cx.observe_window_appearance(window, |this, window, cx| {
            let next_theme = Theme::from_appearance(window.appearance());
            this.theme = next_theme;
            this.filter_input
                .update(cx, |input, cx| input.set_theme(next_theme, cx));
            cx.notify();
        }));

        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.has_been_active = true;
            } else if this.has_been_active && this.dismiss_on_blur {
                window.remove_window();
            }
            cx.notify();
        }));

        let mut this = Self {
            engine,
            available_update,
            update_status: None,
            focus_handle,
            filter_input,
            scroll_handle: UniformListScrollHandle::new(),
            theme,
            active_tab: ViewTab::All,
            query: String::new(),
            items: Vec::new(),
            next_cursor: None,
            has_more: false,
            total_count: 0,
            all_count: 0,
            fav_count: 0,
            selected_index: 0,
            is_loading_initial: true,
            is_loading_more: false,
            generation: 0,
            has_been_active: window.is_window_active(),
            dismiss_on_blur,
            debounce_task: None,
            load_task: None,
            _subscriptions: subscriptions,
        };

        this.reload_first_page(cx);
        this
    }

    /// Called by the app when the background watcher records a new clipboard item while open.
    pub fn notify_clipboard_updated(&mut self, cx: &mut Context<Self>) {
        self.reload_first_page(cx);
    }

    fn on_filter_changed(&mut self, next_query: String, cx: &mut Context<Self>) {
        if self.query == next_query {
            return;
        }
        self.query = next_query;
        self.debounce_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                this.reload_first_page(cx);
            });
        }));
    }

    fn reload_first_page(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.is_loading_initial = true;
        self.is_loading_more = false;
        self.next_cursor = None;

        let engine = self.engine.clone();
        let req = PageRequest {
            tab: self.active_tab,
            query: self.query.clone(),
            cursor: None,
            limit: PAGE_SIZE,
        };

        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let page = engine.query_page(&req)?;
                    let counts = engine.counts()?;
                    anyhow::Ok((page, counts))
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.is_loading_initial = false;
                if let Ok((page, (all_count, fav_count))) = result {
                    this.items = page.items;
                    this.next_cursor = page.next_cursor;
                    this.has_more = page.has_more;
                    this.total_count = page.total_count;
                    this.all_count = all_count;
                    this.fav_count = fav_count;
                    this.selected_index = 0;
                    if !this.items.is_empty() {
                        this.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
                    }
                }
                cx.notify();
            });
        }));
    }

    fn load_next_page(&mut self, cx: &mut Context<Self>) {
        if self.is_loading_more || self.is_loading_initial || !self.has_more {
            return;
        }
        let Some(cursor) = self.next_cursor else {
            self.has_more = false;
            return;
        };

        self.is_loading_more = true;
        let generation = self.generation;
        let engine = self.engine.clone();
        let req = PageRequest {
            tab: self.active_tab,
            query: self.query.clone(),
            cursor: Some(cursor),
            limit: PAGE_SIZE,
        };

        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { engine.query_page(&req) })
                .await;

            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.is_loading_more = false;
                if let Ok(page) = result {
                    this.items.extend(page.items);
                    this.next_cursor = page.next_cursor;
                    this.has_more = page.has_more;
                    this.total_count = page.total_count;
                }
                cx.notify();
            });
        }));
    }

    fn switch_to_tab(&mut self, tab: ViewTab, cx: &mut Context<Self>) {
        if self.active_tab == tab {
            return;
        }
        self.active_tab = tab;
        self.reload_first_page(cx);
    }

    fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }
        if self.selected_index > 0 {
            self.selected_index -= 1;
            self.scroll_handle
                .scroll_to_item(self.selected_index, ScrollStrategy::Top);
            cx.notify();
        }
    }

    fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }
        if self.selected_index + 1 < self.items.len() {
            self.selected_index += 1;
            self.scroll_handle
                .scroll_to_item(self.selected_index, ScrollStrategy::Bottom);
            if self.selected_index + PREFETCH_THRESHOLD >= self.items.len() {
                self.load_next_page(cx);
            }
            cx.notify();
        } else if self.has_more {
            self.load_next_page(cx);
        }
    }

    fn activate_paste(&mut self, _: &ActivatePaste, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_selected(PasteMode::CopyAndPaste, window, cx);
    }

    fn activate_copy_only(
        &mut self,
        _: &ActivateCopyOnly,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_selected(PasteMode::CopyOnly, window, cx);
    }

    fn activate_paste_shift(
        &mut self,
        _: &ActivatePasteShift,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_selected(PasteMode::CopyAndPasteShift, window, cx);
    }

    fn activate_selected(&mut self, mode: PasteMode, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.items.get(self.selected_index) {
            self.activate_by_id(item.id, mode, window, cx);
        }
    }

    fn activate_by_id(
        &mut self,
        id: i64,
        mode: PasteMode,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // Because the GPUI daemon stays alive after the window closes, we write to the clipboard
        // synchronously here (keeping Wayland selection serving alive), close the window so Niri
        // restores focus to the previous app, and emit synthetic Ctrl+V after focus settles.
        let wrote = match self.engine.write_item_to_clipboard(id) {
            Ok(ok) => ok,
            Err(err) => {
                eprintln!("[clipflow] failed to write clipboard item: {err:#}");
                false
            }
        };

        window.remove_window();

        if wrote && mode != PasteMode::CopyOnly {
            std::thread::Builder::new()
                .name("clipflow-autopaste".to_string())
                .spawn(move || {
                    if let Err(err) = trigger_auto_paste(mode, AUTO_PASTE_SETTLE_DELAY) {
                        eprintln!("[clipflow] auto-paste failed (content still copied): {err}");
                    }
                })
                .ok();
        }
    }

    fn toggle_favorite_action(
        &mut self,
        _: &ToggleFavorite,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(item) = self.items.get(self.selected_index) {
            self.toggle_favorite_by_id(item.id, cx);
        }
    }

    fn toggle_favorite_by_id(&mut self, id: i64, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    let toggled = engine.toggle_favorite(id)?;
                    let counts = engine.counts()?;
                    anyhow::Ok((toggled, counts))
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                if let Ok((Some(new_fav), (all_count, fav_count))) = res {
                    this.all_count = all_count;
                    this.fav_count = fav_count;
                    if let Some(pos) = this.items.iter().position(|item| item.id == id) {
                        if this.active_tab == ViewTab::Favorites && !new_fav {
                            this.items.remove(pos);
                            this.total_count = this.total_count.saturating_sub(1);
                            if this.selected_index >= this.items.len() && !this.items.is_empty() {
                                this.selected_index = this.items.len() - 1;
                            }
                        } else {
                            this.items[pos].is_favorite = new_fav;
                        }
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn delete_selected_action(
        &mut self,
        _: &DeleteSelected,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.items.get(self.selected_index) else {
            return;
        };
        let id = item.id;
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    let deleted = engine.delete_item(id)?;
                    let counts = engine.counts()?;
                    anyhow::Ok((deleted, counts))
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                if let Ok((true, (all_count, fav_count))) = res {
                    this.all_count = all_count;
                    this.fav_count = fav_count;
                    if let Some(pos) = this.items.iter().position(|item| item.id == id) {
                        this.items.remove(pos);
                        this.total_count = this.total_count.saturating_sub(1);
                        if this.selected_index >= this.items.len() && !this.items.is_empty() {
                            this.selected_index = this.items.len() - 1;
                        }
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn switch_tab_action(&mut self, _: &SwitchTab, _: &mut Window, cx: &mut Context<Self>) {
        let next = match self.active_tab {
            ViewTab::All => ViewTab::Favorites,
            ViewTab::Favorites => ViewTab::All,
        };
        self.switch_to_tab(next, cx);
    }

    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        let has_query = !self.filter_input.read(cx).is_empty();
        if has_query {
            self.filter_input.update(cx, |input, cx| input.clear(cx));
        } else {
            window.remove_window();
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let has_query = !self.query.is_empty();

        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(12.0))
            .pt(px(12.0))
            .pb(px(10.0))
            .border_b_1()
            .border_color(theme.border)
            .child(
                // Minimalist search bar
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(36.0))
                    .px(px(10.0))
                    .rounded(px(8.0))
                    .bg(theme.surface_subtle)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child("⌕"),
                    )
                    .child(self.filter_input.clone())
                    .when(has_query, |el| {
                        el.child(
                            div()
                                .id("clear-search")
                                .px(px(5.0))
                                .py(px(1.0))
                                .rounded(px(4.0))
                                .text_xs()
                                .text_color(theme.text_secondary)
                                .hover(|s| s.bg(theme.row_hover))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.filter_input.update(cx, |input, cx| input.clear(cx));
                                }))
                                .child("×"),
                        )
                    }),
            )
            .child(
                // Segmented All / Favorites tab strip + subtle count
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(2.0))
                            .p(px(2.0))
                            .rounded(px(7.0))
                            .bg(theme.surface_subtle)
                            .child(self.render_tab_button(
                                "tab-all",
                                ViewTab::All,
                                format!("All ({})", self.all_count),
                                cx,
                            ))
                            .child(self.render_tab_button(
                                "tab-fav",
                                ViewTab::Favorites,
                                format!("★ Favorites ({})", self.fav_count),
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child(if self.has_more {
                                format!("{}/{}", self.items.len(), self.total_count)
                            } else {
                                format!("{} items", self.total_count)
                            }),
                    ),
            )
    }

    fn render_tab_button(
        &self,
        id: &'static str,
        tab: ViewTab,
        label: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let is_active = self.active_tab == tab;

        div()
            .id(id)
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(5.0))
            .text_xs()
            .cursor_pointer()
            .bg(if is_active {
                theme.tab_active_bg
            } else {
                gpui::transparent_black()
            })
            .text_color(if is_active {
                theme.text_primary
            } else {
                theme.text_secondary
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.switch_to_tab(tab, cx);
            }))
            .child(label)
    }

    fn render_row(
        &self,
        ix: usize,
        item: &ClipSummary,
        now_ms: i64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = self.theme;
        let is_selected = ix == self.selected_index;
        let item_id = item.id;
        let is_fav = item.is_favorite;

        let meta_line = match item.kind {
            ClipKind::Text => format!(
                "Text · {} chars · {}",
                item.char_count,
                format_relative_time(now_ms, item.last_copied_at_ms)
            ),
            ClipKind::Image => format!(
                "Image · {}×{} · {} · {}",
                item.image_width.unwrap_or(0),
                item.image_height.unwrap_or(0),
                format_bytes(item.byte_size),
                format_relative_time(now_ms, item.last_copied_at_ms)
            ),
        };

        let left_content = match item.kind {
            ClipKind::Text => div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(2.0))
                .child(
                    div()
                        .w_full()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_sm()
                        .text_color(theme.text_primary)
                        .child(SharedString::from(item.preview_text.clone())),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child(meta_line),
                )
                .into_any_element(),
            ClipKind::Image => {
                let thumb_el = if let Some(path) = item.thumb_path.clone() {
                    img(path)
                        .w(px(40.0))
                        .h(px(40.0))
                        .rounded(px(5.0))
                        .object_fit(ObjectFit::Cover)
                        .with_fallback(move || {
                            div()
                                .w(px(40.0))
                                .h(px(40.0))
                                .rounded(px(5.0))
                                .bg(theme.thumb_bg)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_xs()
                                .text_color(theme.text_muted)
                                .child("IMG")
                                .into_any_element()
                        })
                        .into_any_element()
                } else {
                    div()
                        .w(px(40.0))
                        .h(px(40.0))
                        .rounded(px(5.0))
                        .bg(theme.thumb_bg)
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child("IMG")
                        .into_any_element()
                };

                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(40.0))
                            .h(px(40.0))
                            .rounded(px(5.0))
                            .overflow_hidden()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.thumb_bg)
                            .child(thumb_el),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .justify_center()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.text_primary)
                                    .child(SharedString::from(item.preview_text.clone())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.text_muted)
                                    .child(meta_line),
                            ),
                    )
                    .into_any_element()
            }
        };

        div()
            .id(("clip-row", item_id as usize))
            .h(px(58.0))
            .w_full()
            .px(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .rounded(px(7.0))
            .cursor_pointer()
            .bg(if is_selected {
                theme.row_selected
            } else {
                gpui::transparent_black()
            })
            .hover(move |s| {
                if is_selected {
                    s
                } else {
                    s.bg(theme.row_hover)
                }
            })
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                this.selected_index = ix;
                let mode = if event.modifiers().shift {
                    PasteMode::CopyOnly
                } else if event.modifiers().control {
                    PasteMode::CopyAndPasteShift
                } else {
                    PasteMode::CopyAndPaste
                };
                this.activate_by_id(item_id, mode, window, cx);
            }))
            .child(left_content)
            .child(
                div()
                    .id(("fav-btn", item_id as usize))
                    .flex_none()
                    .w(px(26.0))
                    .h(px(26.0))
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(if is_fav {
                        theme.favorite
                    } else {
                        theme.text_muted
                    })
                    .hover(|s| s.bg(theme.badge_bg))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_favorite_by_id(item_id, cx);
                    }))
                    .child(if is_fav { "★" } else { "☆" }),
            )
            .into_any_element()
    }

    fn trigger_online_update(&mut self, cx: &mut Context<Self>) {
        if self.update_status.is_some() {
            return;
        }
        let Some(info) = self
            .available_update
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        else {
            return;
        };

        self.update_status = Some(format!("Updating to v{}…", info.latest_version));
        cx.notify();

        let shared_update = self.available_update.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { perform_self_update(&info) })
                .await;

            let _ = this.update(cx, |this, cx| match outcome {
                Ok(exe_path) => {
                    if let Ok(mut guard) = shared_update.lock() {
                        *guard = None;
                    }
                    this.update_status = Some("Restarting…".to_string());
                    cx.notify();
                    // Launch the updated binary as daemon and quit the old instance cleanly.
                    let _ = std::process::Command::new(exe_path)
                        .arg("--daemon")
                        .spawn();
                    cx.quit();
                }
                Err(err) => {
                    eprintln!("[clipflow] online update failed: {err:#}");
                    this.update_status = Some("Update failed".to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let update_badge = if let Some(status) = &self.update_status {
            Some(
                div()
                    .px(px(6.0))
                    .py(px(1.0))
                    .rounded(px(4.0))
                    .bg(theme.badge_bg)
                    .text_xs()
                    .text_color(theme.accent)
                    .child(SharedString::from(status.clone()))
                    .into_any_element(),
            )
        } else if let Some(info) = self
            .available_update
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        {
            Some(
                div()
                    .id("update-pill")
                    .px(px(6.0))
                    .py(px(1.0))
                    .rounded(px(4.0))
                    .bg(theme.badge_bg)
                    .text_xs()
                    .text_color(theme.accent)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.row_selected))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.trigger_online_update(cx);
                    }))
                    .child(format!("↑ v{} Update", info.latest_version))
                    .into_any_element(),
            )
        } else {
            None
        };

        div()
            .h(px(30.0))
            .px(px(12.0))
            .border_t_1()
            .border_color(theme.border)
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .text_xs()
            .text_color(theme.text_muted)
            .child("↵ Paste  ·  ⇧↵ Copy  ·  ⌃↵ Term Paste")
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .when_some(update_badge, |el, badge| el.child(badge))
                    .child("⌃S Star  ·  Tab  ·  Esc"),
            )
    }
}

impl Focusable for ClipboardWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ClipboardWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let item_count = self.items.len();

        let body = if item_count == 0 {
            let empty_msg = if self.is_loading_initial {
                "Loading…"
            } else if !self.query.is_empty() {
                "No matching items"
            } else if self.active_tab == ViewTab::Favorites {
                "No favorites yet — press Ctrl+S or click ☆"
            } else {
                "Clipboard history is empty"
            };
            div()
                .flex_1()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.text_muted)
                .child(empty_msg)
                .into_any_element()
        } else {
            let now_ms = now_epoch_ms();
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .px(px(6.0))
                .py(px(6.0))
                .child(
                    uniform_list(
                        "clipflow-list",
                        item_count,
                        cx.processor(
                            move |this, range: std::ops::Range<usize>, _window, cx| {
                            if range.end + PREFETCH_THRESHOLD >= this.items.len()
                                && this.has_more
                                && !this.is_loading_more
                            {
                                this.load_next_page(cx);
                            }

                            let mut rows = Vec::with_capacity(range.len());
                            for ix in range {
                                if let Some(item) = this.items.get(ix) {
                                    rows.push(this.render_row(ix, item, now_ms, cx));
                                }
                            }
                            rows
                        }),
                    )
                    .track_scroll(self.scroll_handle.clone())
                    .h_full(),
                )
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text_primary)
            .border_1()
            .border_color(theme.border)
            .rounded(px(10.0))
            .overflow_hidden()
            .key_context(WINDOW_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::activate_paste))
            .on_action(cx.listener(Self::activate_copy_only))
            .on_action(cx.listener(Self::activate_paste_shift))
            .on_action(cx.listener(Self::toggle_favorite_action))
            .on_action(cx.listener(Self::delete_selected_action))
            .on_action(cx.listener(Self::switch_tab_action))
            .on_action(cx.listener(Self::dismiss))
            .child(self.render_header(cx))
            .child(body)
            .child(self.render_footer(cx))
    }
}

fn format_relative_time(now_ms: i64, then_ms: i64) -> String {
    let diff_secs = (now_ms.saturating_sub(then_ms)) / 1000;
    if diff_secs < 10 {
        "just now".to_string()
    } else if diff_secs < 60 {
        format!("{diff_secs}s ago")
    } else if diff_secs < 3600 {
        format!("{}m ago", diff_secs / 60)
    } else if diff_secs < 86_400 {
        format!("{}h ago", diff_secs / 3600)
    } else {
        format!("{}d ago", diff_secs / 86_400)
    }
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
