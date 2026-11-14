// The context menu of the panes and the menus of their toolbar.
// Methods of App (main.js).

import { sharePermissions } from "./perms.js";
import { prefs, save } from "./prefs.js";
import { t, count } from "./i18n.js";
import { int, parentOf, baseName, isDirLike } from "./format.js";
import { isArchive, archiveStem, isProtectedFolder } from "./icons.js";
import { isBrowsable } from "./archive.js";
import { openMenu, copyText } from "./ui.js";
import { openSettings } from "./settings.js";
import { isRemote } from "./conns.js";
import { isReadOnly, isBackupTop, hasNoSmbPath } from "./backups.js";

export const menuMethods = {
  contextMenu(pane, entry, x, y) {
    const list = pane.targets();
    const many = list.length > 1;
    const atRoot = pane.path === "/";
    const canPaste = this.clipboard && !atRoot;
    const remote = isRemote(pane.path);
    if (!entry) {
      // an archive, or a read-only folder (a PC's backup): nothing is made or pasted
      const ro = isReadOnly(pane);
      const atRootOrRo = atRoot || ro;
      return openMenu(x, y, [
        !atRootOrRo && { label: t("files.common.new_folder"), icon: "folderPlus", key: "Ctrl+Shift+N", action: () => this.newFolder(pane) },
        !atRootOrRo && { label: t("files.menus.upload_files"), icon: "upload", key: "Ctrl+U", action: () => this.pickUpload("files", pane) },
        !atRootOrRo && { label: t("files.menus.upload_folder"), icon: "upload", action: () => this.pickUpload("folder", pane) },
        canPaste && !ro && "-",
        canPaste && !ro && { label: t(this.clipboard.op === "cut" ? "files.menus.paste_move_n" : "files.menus.paste_n", { n: this.clipboard.paths.length }), icon: "paste", key: "Ctrl+V", action: () => this.paste(pane) },
        canPaste && !ro && { label: t(this.clipboard.op === "cut" ? "files.menus.paste_verification_move_n" : "files.menus.paste_verification_n", { n: this.clipboard.paths.length }), icon: "shield", key: "Ctrl+Shift+V", action: () => this.paste(pane, pane.path, true) },
        "-",
        { label: t("files.common.select_all"), key: "Ctrl+A", action: () => pane.selectAll() },
        { label: prefs.showHidden ? t("files.menus.hide_hidden_files") : t("files.common.show_hidden_files"), icon: prefs.showHidden ? "eyeOff" : "eye", key: "Ctrl+H", action: () => this.toggleHidden() },
        !isBackupTop(pane.path) && { label: t("files.menus.compute_folder_sizes"), icon: "drive", action: () => this.computeSizes(pane, pane.visible) },
      pane.path !== "/" && !pane.archive && !remote && !isBackupTop(pane.path) && { label: t("files.common.space_analysis"), icon: "pie", action: () => this.analyseSpace(pane.path) },
        "-",
        !atRoot && { label: t("files.menus.copy_folder_path"), icon: "link", action: () => copyText(this.displayPath(pane.path)) },
        !atRoot && { label: t("files.common.add_favorites"), icon: "star", key: "Ctrl+D", action: () => this.toggleFavorite(pane.path) },
        { label: t("files.common.refresh"), icon: "refresh", key: "F5", action: () => pane.reload() },
        baseName(pane.path) === "#recycle" && !remote && !isReadOnly(pane) && "-",
        baseName(pane.path) === "#recycle" && !remote && !isReadOnly(pane) && { label: t("files.common.empty_recycle_bin"), icon: "trash", danger: true, action: () => this.emptyBin(pane.path) },
      ]);
    }
    const dir = isDirLike(entry);
    const share = !!entry.share;
    // a recycle bin is emptied, not thrown away
    const bin = list.some((e) => e.full !== undefined);
    // a snapshot, or a folder inside one: what is there stays
    // (and the NAS's own folders: #snapshot, #recycle…)
    const fixed = pane.writable === false || list.some((e) => e.readonly || isProtectedFolder(e));
    const shareObj = share ? this.shares.find((s) => "/" + s.name === entry.path) : null;
    const other = this.other();
    // inside an archive: read-only, and folders come out by copying
    const arch = !!pane.archive;
    // …and in a read-only folder (a PC's backup) items are copied out, never changed
    const ro = isReadOnly(pane);
    // a PC's whole backup or a whole snapshot: never measured, analysed or zipped from here
    const whole = isBackupTop(pane.path);
    // the apps' data and the PCs' backups are no SMB shares
    const noSmb = hasNoSmbPath(pane.path);
    const enter = !many && !arch && isBrowsable(entry);
    openMenu(x, y, [
      many ? { title: count(list.length, "files.common.count.items_selected", int) } : null,
      !many && { label: dir || enter ? t("files.common.open") : t("files.common.preview"), icon: dir || enter ? "forward" : "eye", key: dir || enter ? "Enter" : "Space", action: () => this.open(pane, entry) },
      enter && { label: t("files.common.open_in_new_tab"), icon: "plus", action: () => this.newTab(entry.path) },
      !many && dir && { label: t("files.common.open_in_new_tab"), icon: "plus", key: "Ctrl+Enter", action: () => this.newTab(entry.path) },
      !many && dir && other && { label: t("files.common.open_in_other_pane"), icon: "split", action: () => other.go(entry.path) },
      !many && this.editable(pane, entry) && { label: t("files.common.edit"), icon: "rename", key: "F4", action: () => this.edit(pane, entry) },
      (!arch || (!many && !dir)) && !(whole && (many || dir)) && { label: many || dir ? t("files.menus.download_zip") : t("files.common.download"), icon: "download", action: () => this.download(list) },
      !many && !arch && !remote && isArchive(entry) && "-",
      !many && !ro && !remote && isArchive(entry) && { label: t("files.menus.extract_x", { x: archiveStem(entry.name) }), icon: "archive", action: () => this.extract(pane, entry, "folder") },
      !many && !ro && !remote && isArchive(entry) && { label: t("files.common.extract_here"), icon: "archive", action: () => this.extract(pane, entry, "here") },
      !many && !arch && !remote && isArchive(entry) && { label: t("files.menus.extract"), icon: "archive", action: () => this.extract(pane, entry, "pick") },
      !share && !ro && !remote && { label: t("files.menus.compress"), icon: "archive", action: () => this.compress(pane) },
      "-",
      !share && { label: t("files.common.copy"), icon: "copy", key: "Ctrl+C", action: () => this.setClipboard("copy", pane) },
      !share && !ro && !fixed && { label: t("files.common.cut"), icon: "cut", key: "Ctrl+X", action: () => this.setClipboard("cut", pane) },
      dir && canPaste && !many && !ro && { label: t("files.menus.paste"), icon: "paste", action: () => this.paste(pane, entry.path) },
      !share && { label: t("files.menus.copy"), icon: "copy", key: other ? "F5" : "", action: () => (other ? this.toOtherPane("copy") : this.copyTo(pane, "copy")) },
      !share && !ro && !fixed && { label: t("files.menus.move"), icon: "move", key: other ? "F6" : "", action: () => (other ? this.toOtherPane("move") : this.copyTo(pane, "move")) },
      !share && { label: t("files.menus.copy_verification"), icon: "shield", key: "Shift+F5", action: () => (other ? this.toOtherPane("copy", true) : this.copyTo(pane, "copy", true)) },
      !share && !ro && !fixed && { label: t("files.menus.move_verification"), icon: "shield", key: "Shift+F6", action: () => (other ? this.toOtherPane("move", true) : this.copyTo(pane, "move", true)) },
      !share && other && { label: t("files.menus.choose_folder"), icon: "move", action: () => this.copyTo(pane, "copy") },
      "-",
      !many && !share && !ro && !fixed && { label: t("files.common.rename"), icon: "rename", key: "F2", action: () => this.renamePrompt(pane, entry) },
      !share && !bin && !ro && !fixed && { label: pane.recycle && !pane.path.includes("#recycle") ? t("files.common.move_recycle_bin") : t("files.common.delete"), icon: "trash", key: "Del", danger: true, action: () => this.remove(pane) },
      !share && !bin && !ro && !fixed && pane.recycle && { label: t("files.common.delete_permanently"), icon: "trash", key: "Shift+Del", danger: true, action: () => this.remove(pane, true) },
      !many && entry.full !== undefined && !ro && !remote && { label: t("files.common.empty_recycle_bin"), icon: "trash", danger: true, action: () => this.emptyBin(entry.path) },
      "-",
      dir && !whole && { label: t("files.menus.compute_size"), icon: "drive", action: () => this.computeSizes(pane, list) },
      !many && dir && !arch && !remote && !whole && { label: t("files.common.space_analysis"), icon: "pie", action: () => this.analyseSpace(entry.path) },
      !remote && { label: t("files.common.copy_path"), icon: "link", key: "Ctrl+Shift+C", action: () => this.copyPaths(list, "nas") },
      !remote && !noSmb && { label: t("files.menus.copy_network_path_nas"), icon: "link", action: () => this.copyPaths(list, "smb") },
      remote && { label: t("files.common.copy_address"), icon: "link", key: "Ctrl+Shift+C", action: () => this.copyPaths(list, "smb") },
      (!remote || this.connOf(pane.path)?.kind === "smb") && !noSmb && { label: t("files.menus.copy_smb_address"), icon: "link", action: () => this.copyPaths(list, "url") },
      !many && dir && { label: t("files.common.add_favorites"), icon: "star", action: () => this.toggleFavorite(entry.path) },
      { label: t("files.menus.properties"), icon: "info", key: "Ctrl+I", action: () => !prefs.info && this.toggleInfo() },
      !many && shareObj && !shareObj.home && shareObj.volume && !this.removableShare(entry.path) && { label: t("files.common.permissions_menu"), icon: "shield", action: () => sharePermissions(shareObj) },
      !many && share && this.removableShare(entry.path) && "-",
      !many && share && this.removableShare(entry.path) && { label: t("files.common.eject"), icon: "eject", action: () => this.eject(entry.name) },
    ]);
  },

  newMenu(pane, anchor) {
    const r = anchor.getBoundingClientRect();
    const disabled = pane.path === "/" || isReadOnly(pane);
    openMenu(r.left, r.bottom + 4, [
      { label: t("files.common.new_folder"), icon: "folderPlus", key: "Ctrl+Shift+N", disabled, action: () => this.newFolder(pane) },
      { label: t("files.menus.upload_files"), icon: "upload", key: "Ctrl+U", disabled, action: () => this.pickUpload("files", pane) },
      { label: t("files.menus.upload_folder"), icon: "upload", disabled, action: () => this.pickUpload("folder", pane) },
      "-",
      { label: t("files.common.new_tab"), icon: "plus", key: "Ctrl+T", action: () => this.newTab(this.newTabPath()) },
    ]);
  },

  moreMenu(pane, anchor) {
    const r = anchor.getBoundingClientRect();
    const fav = (prefs.favorites || []).some((f) => f.path === pane.path);
    openMenu(r.right - 240, r.bottom + 4, [
      { label: t("files.common.select_all"), key: "Ctrl+A", action: () => pane.selectAll() },
      { label: t("files.common.invert_selection"), key: "Ctrl+Shift+I", action: () => pane.invertSelection() },
      this.clipboard && !isReadOnly(pane) && { label: t("files.common.paste"), icon: "paste", key: "Ctrl+V", action: () => this.paste(pane) },
      this.clipboard && !isReadOnly(pane) && { label: t("files.common.paste_verification"), icon: "shield", key: "Ctrl+Shift+V", action: () => this.paste(pane, pane.path, true) },
      "-",
      { title: t("files.menus.sort") },
      ...[["name", "files.common.name"], ["size", "files.common.size"], ["mtime", "files.common.modified"], ["kind", "files.common.type"]].map(([k, l]) => ({
        label: t(l) + (pane.sort.key === k ? (pane.sort.dir > 0 ? "  ▲" : "  ▼") : ""),
        action: () => pane.setSort(k),
      })),
      { label: (prefs.foldersFirst ? "✓ " : "") + t("files.common.folders_first"), action: () => save({ foldersFirst: !prefs.foldersFirst }) },
      "-",
      { label: (prefs.sidebar ? "✓ " : "") + t("files.common.sidebar"), icon: "sidebar", key: "Ctrl+B", action: () => save({ sidebar: !prefs.sidebar }) },
      !isBackupTop(pane.path) && { label: t("files.menus.compute_folder_sizes"), icon: "drive", action: () => this.computeSizes(pane, pane.visible) },
      pane.path !== "/" && !pane.archive && !isRemote(pane.path) && !isBackupTop(pane.path) && { label: t("files.common.space_analysis"), icon: "pie", action: () => this.analyseSpace(pane.path) },
      pane.path !== "/" && { label: fav ? t("files.common.remove_favorites") : t("files.common.add_favorites"), icon: "star", key: "Ctrl+D", action: () => this.toggleFavorite(pane.path) },
      "-",
      { label: t("files.menus.keyboard_shortcuts"), icon: "keyboard", key: "?", action: () => openSettings("keys") },
      { label: t("files.common.settings"), icon: "settings", key: "Ctrl+,", action: () => openSettings() },
    ]);
  },

  columnsMenu(x, y) {
    const c = prefs.columns;
    const toggle = (k) => save({ columns: { ...c, [k]: !c[k] } });
    openMenu(x, y, [
      { title: t("files.common.columns") },
      { label: (c.mode ? "✓ " : "") + t("files.common.permissions"), action: () => toggle("mode") },
      { label: (c.size ? "✓ " : "") + t("files.common.size"), action: () => toggle("size") },
      { label: (c.mtime ? "✓ " : "") + t("files.common.modified"), action: () => toggle("mtime") },
      { label: (c.kind ? "✓ " : "") + t("files.common.type"), action: () => toggle("kind") },
    ]);
  },
};
