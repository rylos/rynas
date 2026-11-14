// The settings dialog: look, list, behaviour, transfers, keys.

import { prefs, save, DEFAULTS } from "./prefs.js";
import { t, keyText } from "./i18n.js";
import { svg } from "./icons.js";
import { dialog, $, h, pickFolder } from "./ui.js";
import { esc } from "./format.js";

const SECTIONS = [
  ["look", "palette", "files.settings.appearance"],
  ["list", "list", "files.common.list"],
  ["behaviour", "settings", "files.settings.behavior"],
  ["transfers", "transfers", "files.common.transfers"],
  ["keys", "keyboard", "files.settings.shortcuts"],
];

export const KEYS = [
  ["Ctrl+T", "files.common.new_tab"], ["Ctrl+W", "files.common.close_tab"], ["Ctrl+Tab / Ctrl+PgDn", "files.settings.next_tab"],
  ["Ctrl+Shift+Tab / Ctrl+PgUp", "files.settings.previous_tab"], ["Alt+1…9", "files.settings.go_tab"], ["Ctrl+Shift+T", "files.settings.reopen_closed_tab"],
  ["F3", "files.common.dual_pane"], ["Tab", "files.settings.other_pane_dual_pane"], ["F5 / F6", "files.settings.copy_move_other_pane"], ["Shift+F5 / Shift+F6", "files.settings.copy_move_verification"], ["Ctrl+Shift+V", "files.common.paste_verification"],
  ["Enter", "files.common.open"], ["Backspace / Alt+↑", "files.common.parent_folder"], ["Alt+← / Alt+→", "files.settings.back_forward"],
  ["Space", "files.settings.quick_look"], ["Ctrl+I", "files.common.info_panel"], ["Ctrl+H", "files.settings.show_hide_hidden_files"],
  ["Ctrl+L", "files.settings.type_path"], ["/", "files.settings.fuzzy_filter_folder"], ["Ctrl+F", "files.settings.search_subfolders"],
  ["Ctrl+1 / Ctrl+2 / Ctrl+3", "files.settings.list_grid_columns"], ["Ctrl+B", "files.common.sidebar"], ["Ctrl+R", "files.common.refresh"],
  ["Ctrl+C / Ctrl+X / Ctrl+V", "files.settings.copy_cut_paste"], ["Ctrl+A", "files.common.select_all"], ["Ctrl+Shift+I", "files.common.invert_selection"],
  ["Insert", "files.settings.select_move_on"], ["F2", "files.common.rename"], ["Ctrl+Shift+N", "files.common.new_folder"],
  ["Del", "files.common.move_recycle_bin"], ["Shift+Del", "files.common.delete_permanently"], ["Ctrl+D", "files.common.add_favorites"],
  ["Ctrl+Shift+C", "files.common.copy_path"], ["Ctrl+U", "files.settings.upload_files"], ["Ctrl+J", "files.common.transfers"], ["Ctrl+,", "files.common.settings"],
  ["Letters", "files.settings.jump_name_starting_like"], ["j k h l, gg, G", "files.settings.vim_keys_on"],
];

function row(label, hint, control) {
  return `<div class="set-row"><div class="lbl"><span>${t(label)}</span>${hint ? `<small>${t(hint)}</small>` : ""}</div>${control}</div>`;
}
const sw = (key) => `<label class="switch"><input type="checkbox" data-k="${key}" ${getK(key) ? "checked" : ""}><span></span></label>`;
const sel = (key, options) =>
  `<select class="input" data-k="${key}">${options.map(([v, l]) => `<option value="${v}" ${String(getK(key)) === String(v) ? "selected" : ""}>${l.startsWith("files.") ? t(l) : esc(l)}</option>`).join("")}</select>`;

function getK(key) {
  return key.split(".").reduce((o, k) => o?.[k], prefs);
}
function setK(key, value) {
  const parts = key.split(".");
  if (parts.length === 1) return save({ [key]: value });
  const top = { ...prefs[parts[0]], [parts[1]]: value };
  save({ [parts[0]]: top });
}

function body(section) {
  switch (section) {
    case "look":
      return (
        row("files.settings.file_icons", "files.settings.eza_icons_nerd_font_type", sw("icons")) +
        row("files.settings.name_colors", "files.settings.eza_colors_blue_folders_purple", sw("colors")) +
        row("files.settings.system_folders_in_gray", "files.settings.recycle_snapshot_recycle_bin_lost", sw("greySystem")) +
        row("files.settings.ll_style_columns", "files.settings.permissions_sizes_dates_colored_like", sw("llColumns")) +
        row("files.settings.text_size", "", sel("size", [["small", "files.settings.small"], ["normal", "files.common.normal"], ["large", "files.settings.large"]])) +
        row("files.settings.row_density", "", sel("density", [["compact", "files.settings.compact"], ["normal", "files.common.normal"]]))
      );
    case "list":
      return (
        row("files.common.folders_first", "files.settings.like_eza_group_directories_first", sw("foldersFirst")) +
        row("files.common.show_hidden_files", "files.settings.those_starting_dot_ctrl_h", sw("showHidden")) +
        row("files.settings.hidden_files_last", "files.settings.shown_after_others", sw("hiddenLast")) +
        row("files.settings.show_favorites", "files.settings.favorites_section_sidebar_stay_saved", sw("showFavorites")) +
        row("files.settings.volume_names", "Quelli dati ai volumi (es. «HDD»), altrimenti «Volume 1»", sw("volumeNames")) +
        row("files.settings.remember_view_sort_each_folder", "files.settings.otherwise_last_ones_used_apply", sw("perFolder")) +
        row("files.settings.highlight_today_dates", "files.settings.in_modified_column", sw("todayHighlight")) +
        row("files.settings.binary_sizes", "files.settings.kib_mib_gib_1024_instead", sw("binarySizes")) +
        row("files.settings.relative_dates", "files.settings.relative_dates_hint", sw("relativeDates")) +
        row("files.settings.permissions_column", "", sw("columns.mode")) +
        row("files.settings.size_column", "", sw("columns.size")) +
        row("files.settings.modified_column", "", sw("columns.mtime")) +
        row("files.settings.type_column", "", sw("columns.kind"))
      );
    case "behaviour":
      return (
        row("files.settings.open_one_click", "files.settings.otherwise_double_click", sw("singleClick")) +
        row("files.settings.vim_keys", "files.settings.j_k_move_h_go", sw("vimKeys")) +
        row("files.settings.confirm_before_deleting", "", sw("confirmDelete")) +
        row("files.settings.on_start_open", "", sel("startIn", [["last", "files.settings.last_tabs"], ["shares", "files.common.shared_folders"], ["folder", "files.settings.the_chosen_folder"]])) +
        row("files.settings.new_tabs_open_in", "files.settings.ctrl_t_tab_bar", sel("newTabIn", [["current", "files.settings.current_folder"], ["shares", "files.common.shared_folders"], ["folder", "files.settings.the_chosen_folder"]])) +
        row("files.settings.chosen_folder", "", `<button class="btn small pick-start">${prefs.startFolder ? `<span style="font-family:var(--mono)">${esc(prefs.startFolder)}</span>` : t("files.settings.choose")}</button>`)
      );
    case "transfers":
      return (
        row("files.settings.item_already_exists", "files.settings.starting_choice_copies_moves_uploads", sel("conflict", [["ask", "files.settings.ask"], ["rename", "files.common.keep_both"], ["skip", "files.common.skip"], ["overwrite", "files.common.overwrite"], ["newer", "files.common.overwrite_newer"]])) +
        row("files.common.speed_limit", "files.settings.can_changed_during_transfer", sel("limit", [[0, "files.settings.none"], [10, "10 MiB/s"], [25, "25 MiB/s"], [50, "50 MiB/s"], [100, "100 MiB/s"], [200, "200 MiB/s"]])) +
        row("files.settings.open_dialog_on_start", "files.settings.otherwise_transfer_starts_in_background", sw("openDialog")) +
        row("files.settings.browser_notification_end", "files.settings.page_not_in_front", sw("notify"))
      );
    case "keys":
      return `<table class="keys">${KEYS.map(([k, d]) => `<tr><td>${keyText(k).split(" / ").map((x) => x.split("+").map((p) => `<kbd>${esc(p)}</kbd>`).join("+")).join(" / ")}</td><td>${t(d)}</td></tr>`).join("")}</table>`;
  }
  return "";
}


export function openSettings(start = "look") {
  const dlg = dialog(`
    <div class="dlg-head"><div class="dlg-icon">${svg("settings")}</div><h2>${t("files.settings.files_settings")}</h2><button class="icon-btn x" aria-label="${t("files.common.close")}">${svg("close")}</button></div>
    <div class="settings"><nav>${SECTIONS.map(([k, i, l]) => `<button data-s="${k}">${svg(i)} ${t(l)}</button>`).join("")}</nav><section></section></div>
    <div class="dlg-foot"><div class="left"><button class="btn ghost reset">${t("files.settings.reset_defaults")}</button></div><button class="btn primary ok">${t("files.settings.done")}</button></div>`, { wide: true });
  const section = $("section", dlg);
  const show = (s) => {
    dlg.querySelectorAll("nav button").forEach((b) => b.classList.toggle("on", b.dataset.s === s));
    section.innerHTML = body(s);
    section.querySelectorAll("[data-k]").forEach((input) => {
      input.onchange = () => {
        const v = input.type === "checkbox" ? input.checked : input.dataset.k === "limit" ? +input.value : input.value;
        setK(input.dataset.k, v);
        if (input.dataset.k === "notify" && v && "Notification" in window) Notification.requestPermission();
      };
    });
    const pick = $(".pick-start", section);
    if (pick) {
      pick.onclick = async () => {
        const path = await pickFolder({ title: t("files.settings.chosen_folder"), start: prefs.startFolder || "/", ok: t("files.settings.use_folder") });
        if (!path) return;
        save({ startFolder: path });
        show(s);
      };
    }
  };
  dlg.querySelectorAll("nav button").forEach((b) => (b.onclick = () => show(b.dataset.s)));
  $(".x", dlg).onclick = () => dlg.close();
  $(".ok", dlg).onclick = () => dlg.close();
  $(".reset", dlg).onclick = () => {
    const keep = { favorites: prefs.favorites, tabs: prefs.tabs };
    save({ ...structuredClone(DEFAULTS), ...keep });
    show(dlg.querySelector("nav .on")?.dataset.s || "look");
  };
  show(start);
}
