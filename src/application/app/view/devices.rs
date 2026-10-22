//! Shared device-list presentation. Selection and actions always use device IDs.
use super::*;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Filter {
    #[default]
    All,
    Online,
    Offline,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Sort {
    #[default]
    Status,
    Name,
}

impl Sort {
    fn label(self) -> &'static str {
        match self {
            Self::Status => "在线优先",
            Self::Name => "名称排序",
        }
    }
}

#[derive(Default)]
pub(super) struct ListUi {
    search: String,
    filter: Filter,
    sort: Sort,
    /// 选中的标签，全部命中才显示；一个都没选就是不过滤。
    tags: Vec<String>,
}

impl ListUi {
    pub(super) fn clear_tags(&mut self) {
        self.tags.clear();
    }
    fn toggle_tag(&mut self, tag: &str) {
        match self.tags.iter().position(|existing| existing == tag) {
            Some(index) => {
                self.tags.remove(index);
            }
            None => self.tags.push(tag.to_owned()),
        }
    }
}

struct Entry {
    device: DeviceInfo,
    group: usize,
    current: bool,
    viewing: bool,
    own_session: bool,
    connect_issue: Option<String>,
    takeover: bool,
    status: crate::application::app::device_status::DeviceStatus,
}

enum Action {
    Details,
    Connect,
    Audio,
}

impl DeviceCenterApp {
    pub(super) fn devices_page(&mut self, ui: &mut egui::Ui) {
        self.device_list(ui, false);
    }
    pub(super) fn management_page(&mut self, ui: &mut egui::Ui) {
        self.device_list(ui, true);
    }

    /// 设备行上的标签及数量。已选中但当前没有设备的标签也留着，否则就没法取消。
    fn tag_counts(
        &self,
        rows: Option<&[Entry]>,
        selected: &[String],
    ) -> std::collections::BTreeMap<String, usize> {
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        let Some(store) = self.tags.as_ref() else {
            return counts;
        };
        for entry in rows.into_iter().flatten() {
            for tag in store.tags(&entry.device.device_id) {
                *counts.entry(tag.clone()).or_default() += 1;
            }
        }
        for tag in selected {
            counts.entry(tag.clone()).or_default();
        }
        counts
    }

    /// 详情页那个输入框的文本。换设备时先把上一台的草稿落盘，再按存储里的值重建。
    pub(super) fn tag_draft(&mut self, id: &str) -> &mut String {
        if !self.tag_draft.as_ref().is_some_and(|draft| draft.id == id) {
            self.flush_tag_draft();
            let text = self
                .tags
                .as_ref()
                .map(|store| store.tags(id).join(" "))
                .unwrap_or_default();
            self.tag_draft = Some(TagDraft {
                id: id.to_owned(),
                text,
            });
        }
        &mut self
            .tag_draft
            .as_mut()
            .expect("草稿刚刚建好")
            .text
    }

    /// 把草稿写回标签存储。文字没变就不写，免得每次失焦都落一次盘。
    pub(super) fn flush_tag_draft(&mut self) {
        let Some(draft) = self.tag_draft.take() else {
            return;
        };
        let parsed = tags::parse(&draft.text);
        let Some(store) = self.tags.as_mut() else {
            return;
        };
        if parsed.as_slice() == store.tags(&draft.id) {
            return;
        }
        if let Err(error) = store.set(&draft.id, parsed) {
            self.status = StatusMessage::error(format!("保存标签失败：{error:#}"));
        }
    }

    pub(super) fn open_tag_sync(&mut self) {
        let draft = self
            .tags
            .as_ref()
            .and_then(|store| store.sync_dir())
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.tag_sync = Some(draft);
    }

    fn device_entries(&self, management: bool) -> Option<Vec<Entry>> {
        let mut devices = Vec::new();
        if management {
            let catalog = self.catalog.as_ref()?;
            devices.extend(catalog.groups.desktop_devices.iter().map(|d| (d, 0)));
            devices.extend(catalog.groups.mobile_devices.iter().map(|d| (d, 1)));
            devices.extend(catalog.groups.tv_devices.iter().map(|d| (d, 2)));
        } else {
            devices.extend(
                all_devices(self.devices.as_ref()?)
                    .filter(|(_, d)| self.show_in_watching_list(d))
                    .map(|(_, d)| (d, 0)),
            );
        }
        Some(
            devices
                .into_iter()
                .map(|(device, group)| {
                    let id = device.device_id.as_str();
                    let own_session = self.viewer_for_device(id).is_some();
                    let connect_issue = self.viewer_action_issue(device);
                    Entry {
                        device: device.clone(),
                        group,
                        current: self
                            .catalog
                            .as_ref()
                            .is_some_and(|c| c.groups.current_device_id == id),
                        viewing: !management && self.is_viewing_target(id),
                        own_session,
                        connect_issue,
                        takeover: self.needs_takeover(device),
                        status: self.device_status(device),
                    }
                })
                .collect(),
        )
    }

    fn device_list(&mut self, ui: &mut egui::Ui, management: bool) {
        let rows = self.device_entries(management);
        let index = usize::from(management);
        let count = rows.as_ref().map_or(0, Vec::len);
        let online = rows
            .as_ref()
            .map_or(0, |r| r.iter().filter(|e| e.device.is_connected()).count());
        let offline = rows.as_ref().map_or(0, |r| {
            r.iter()
                .filter(|e| e.device.status == "DISCONNECTED")
                .count()
        });
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(self.center_ui.page.title())
                        .size(crate::ui::theme::TITLE)
                        .strong(),
                );
                ui.label(
                    RichText::new(if management {
                        "账号中的电脑、手机、平板与电视"
                    } else {
                        "选择电脑，开始远程连接"
                    })
                    .color(MUTED),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let response = ui.add_enabled(
                    !self.refresh_pending && !self.logout_pending,
                    egui::Button::new(if self.refresh_pending {
                        "刷新中…"
                    } else {
                        "刷新设备"
                    }),
                );
                if response.clicked() {
                    self.request_refresh();
                }
                if let Some(at) = self.refreshed_at {
                    response
                        .on_hover_text(format!("上次状态更新于 {} 秒前", at.elapsed().as_secs()));
                }
                if ui.button("标签同步").clicked() {
                    self.open_tag_sync();
                }
                if !management {
                    self.active_view(ui);
                }
            });
        });
        ui.add_space(20.0);
        let state = &mut self.center_ui.device_lists[index];
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 182.0).max(140.0);
            ui.add_sized(
                [width, 36.0],
                singleline_input(&mut state.search).hint_text("搜索设备名称、ID、系统或版本"),
            );
            if ui
                .add_enabled(!state.search.is_empty(), egui::Button::new("清除"))
                .clicked()
            {
                state.search.clear();
            }
            egui::ComboBox::from_id_salt(("device-sort", index))
                .width(108.0)
                .selected_text(state.sort.label())
                .show_ui(ui, |ui| {
                    for sort in [Sort::Status, Sort::Name] {
                        ui.selectable_value(&mut state.sort, sort, sort.label());
                    }
                });
        });
        ui.add_space(8.0);
        // 状态和标签同一行；标签多的账号这里会自然折到第二行。
        let active_tags = self.center_ui.device_lists[index].tags.clone();
        let tag_counts = self.tag_counts(rows.as_deref(), &active_tags);
        let state = &mut self.center_ui.device_lists[index];
        ui.horizontal_wrapped(|ui| {
            for (filter, label, n) in [
                (Filter::All, "全部", count),
                (Filter::Online, "在线", online),
                (Filter::Offline, "离线", offline),
            ] {
                ui.selectable_value(&mut state.filter, filter, format!("{label}  {n}"));
            }
            if !tag_counts.is_empty() {
                ui.add_space(8.0);
                for (tag, n) in &tag_counts {
                    let picked = state.tags.contains(tag);
                    if ui
                        .selectable_label(picked, format!("#{tag}  {n}"))
                        .clicked()
                    {
                        state.toggle_tag(tag);
                    }
                }
                if !state.tags.is_empty() && ui.button("清除标签").clicked() {
                    state.clear_tags();
                }
            }
        });
        let query = state.search.trim().to_lowercase();
        let filter = state.filter;
        let sort = state.sort;
        ui.add_space(16.0);
        self.alert(ui);
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "catalog-error",
            "设备清单未更新",
            crate::ui::controls::DialogIcon::Warning,
            self.catalog_error.as_deref().filter(|_| management),
        );
        let Some(mut rows) = rows else {
            self.empty_state(ui, "正在读取设备清单…", None);
            return;
        };
        // 芯片行里可能刚被点掉一个标签，取最新的一次。
        let selected_tags = self.center_ui.device_lists[index].tags.clone();
        let store = self.tags.as_ref();
        rows.retain(|e| {
            let d = &e.device;
            let tags = store.map_or(&[][..], |store| store.tags(&d.device_id));
            let visible = match filter {
                Filter::All => true,
                Filter::Online => d.is_connected(),
                Filter::Offline => d.status == "DISCONNECTED",
            };
            visible
                && selected_tags
                    .iter()
                    .all(|wanted| tags.iter().any(|tag| tag == wanted))
                && (query.is_empty()
                    || [
                        display_alias(d),
                        &d.device_id,
                        &d.platform_label(),
                        &d.version_name,
                    ]
                    .iter()
                    .any(|value| value.to_lowercase().contains(&query))
                    || tags.iter().any(|tag| tag.to_lowercase().contains(&query)))
        });
        rows.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then_with(|| {
                    if sort == Sort::Status {
                        b.device.is_connected().cmp(&a.device.is_connected())
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then_with(|| {
                    display_alias(&a.device)
                        .to_lowercase()
                        .cmp(&display_alias(&b.device).to_lowercase())
                })
                .then_with(|| a.device.device_id.cmp(&b.device.device_id))
        });
        if rows.is_empty() {
            let clear_filters = self.empty_state(
                ui,
                if count == 0 {
                    "暂无设备"
                } else {
                    "没有符合条件的设备"
                },
                (count > 0).then_some("清除搜索和筛选"),
            );
            if clear_filters {
                let state = &mut self.center_ui.device_lists[index];
                state.search.clear();
                state.filter = Filter::All;
                state.clear_tags();
            }
            return;
        }
        let mut picked = None;
        crate::ui::controls::page_scroll(("device-list", index)).show(ui, |ui| {
            let groups = ["电脑", "手机 / 平板", "电视"];
            for (group, title) in groups.iter().enumerate() {
                let entries = rows.iter().filter(|e| e.group == group).collect::<Vec<_>>();
                if entries.is_empty() {
                    continue;
                }
                if management {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(*title).strong().color(MUTED));
                        ui.label(
                            RichText::new(entries.len().to_string())
                                .small()
                                .color(MUTED),
                        );
                    });
                    ui.add_space(6.0);
                }
                for entry in entries {
                    let selected =
                        self.selected_device_id.as_deref() == Some(&entry.device.device_id);
                    let tags = store.map(|store| store.tags(&entry.device.device_id));
                    if let Some(action) = ui
                        .push_id(&entry.device.device_id, |ui| {
                            row(ui, entry, selected, tags, &mut self.center_ui.wallpapers)
                        })
                        .inner
                    {
                        picked = Some((entry.device.device_id.clone(), action));
                    }
                    ui.add_space(6.0);
                }
                ui.add_space(12.0);
            }
        });
        if let Some((id, action)) = picked {
            match action {
                Action::Details => self.open_details(id),
                Action::Connect => {
                    self.selected_device_id = Some(id);
                    self.start_viewer();
                }
                Action::Audio => {
                    self.selected_device_id = Some(id);
                    self.start_viewer_mode(true);
                }
            }
        }
    }
}

fn row(
    ui: &mut egui::Ui,
    entry: &Entry,
    selected: bool,
    tags: Option<&[String]>,
    wallpapers: &mut crate::application::wallpaper::Wallpapers,
) -> Option<Action> {
    let device = &entry.device;
    let wide = ui.available_width() >= 760.0;
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), if wide { 88.0 } else { 112.0 }),
        Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return None;
    }
    let texture = wallpapers.texture(ui.ctx(), &device.device_id, &device.wallpaper_url);
    let fill = if selected {
        crate::ui::theme::SELECTED
    } else if response.hovered() {
        crate::ui::theme::HOVER
    } else {
        SURFACE
    };
    let p = ui.painter();
    p.rect_filled(rect, 8.0, fill);
    p.rect_stroke(
        rect,
        8.0,
        Stroke::new(
            1.0,
            if selected {
                crate::ui::theme::BORDER_FOCUS
            } else {
                LINE
            },
        ),
        egui::StrokeKind::Inside,
    );
    let preview = egui::Rect::from_min_size(rect.min + vec2(14.0, 12.0), vec2(116.0, 64.0));
    device_visuals::wallpaper(ui, preview, texture.as_ref(), device.platform);
    let icon =
        egui::Rect::from_min_size(preview.right_bottom() - vec2(29.0, 29.0), vec2(26.0, 26.0));
    ui.painter().rect_filled(icon, 5.0, BG);
    device_visuals::system_icon(ui.painter(), icon.shrink(3.0), device.platform);
    let actions_x = rect.right() - if entry.viewing { 210.0 } else { 92.0 };
    let status_x = if wide {
        actions_x - 120.0
    } else {
        rect.left() + 148.0
    };
    let metadata_x = if wide {
        status_x - 154.0
    } else {
        rect.left() + 260.0
    };
    let name_right = if wide {
        metadata_x - 16.0
    } else {
        actions_x - 12.0
    };
    let mut name = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_max(
        rect.min + vec2(148.0, 18.0),
        egui::pos2(name_right, rect.top() + 70.0),
    )));
    name.add(
        egui::Label::new(
            RichText::new(display_alias(device))
                .size(crate::ui::theme::SECTION)
                .strong(),
        )
        .truncate(),
    )
    .on_hover_text(display_alias(device));
    // 标签跟着 ID 一行显示，不额外占高度；太长的部分由 truncate 截掉。
    let mut id_line = format!(
        "{}{}",
        if entry.current { "本机 · " } else { "ID  " },
        device.device_id
    );
    for tag in tags.into_iter().flatten() {
        id_line.push_str("  #");
        id_line.push_str(tag);
    }
    name.add(
        egui::Label::new(
            RichText::new(id_line.clone())
                .size(crate::ui::theme::TINY)
                .color(MUTED),
        )
        .truncate(),
    )
    .on_hover_text(id_line);
    let status_rect = egui::Rect::from_min_size(
        egui::pos2(status_x, rect.top() + if wide { 30.0 } else { 77.0 }),
        crate::ui::theme::DEVICE_STATUS_SIZE.into(),
    );
    crate::ui::controls::device_status_badge(
        ui,
        status_rect,
        entry.status.label(),
        entry.status.color(),
    );
    let mut metadata = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_max(
        egui::pos2(metadata_x, rect.top() + if wide { 22.0 } else { 82.0 }),
        egui::pos2(
            if wide {
                status_x - 12.0
            } else {
                rect.right() - 14.0
            },
            rect.bottom() - 8.0,
        ),
    )));
    if wide {
        metadata.label(RichText::new(device.platform_label()).size(crate::ui::theme::COMPACT_TEXT));
        metadata.add(
            egui::Label::new(
                RichText::new(if device.version_name.is_empty() {
                    "版本未知"
                } else {
                    &device.version_name
                })
                .size(crate::ui::theme::TINY)
                .color(MUTED),
            )
            .truncate(),
        );
    } else {
        metadata.add(
            egui::Label::new(
                RichText::new(format!(
                    "{} · {}",
                    device.platform_label(),
                    if device.version_name.is_empty() {
                        "版本未知"
                    } else {
                        &device.version_name
                    }
                ))
                .size(crate::ui::theme::TINY)
                .color(MUTED),
            )
            .truncate(),
        );
    }
    let mut action = None;
    let mut buttons = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(egui::Rect::from_min_max(
                egui::pos2(actions_x, rect.top() + 12.0),
                egui::pos2(
                    rect.right() - 14.0,
                    rect.top() + if wide { 76.0 } else { 70.0 },
                ),
            ))
            .layout(egui::Layout::right_to_left(Align::Center)),
    );
    if entry.viewing {
        let button = buttons.add_enabled(
            entry.connect_issue.is_none(),
            primary(if entry.own_session {
                "已打开"
            } else if entry.takeover {
                "接管"
            } else {
                "连接"
            }),
        );
        if button.clicked() {
            action = Some(Action::Connect);
        }
        let audio = buttons
            .add_enabled_ui(entry.connect_issue.is_none(), |ui| {
                crate::ui::controls::audio_output_button(
                    ui,
                    "仅音频连接",
                    crate::ui::theme::CONTROL_HEIGHT,
                )
            })
            .inner;
        if audio.clicked() {
            action = Some(Action::Audio);
        }
        if let Some(issue) = &entry.connect_issue {
            audio.on_disabled_hover_text(issue);
        }
        if let Some(issue) = &entry.connect_issue {
            button.on_disabled_hover_text(issue);
        }
    }
    if buttons.button("详情").clicked() {
        action = Some(Action::Details);
    }
    action.or_else(|| response.clicked().then_some(Action::Details))
}
