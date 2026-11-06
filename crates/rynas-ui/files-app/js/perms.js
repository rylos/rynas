// The settings of a shared folder (description, permissions) live on the
// rynas Shares page, with the permission matrix and the explained effective
// permission: the context menus open it at the right share and tab.

import { navigate } from "./host.js";

// tabs of the share editor (crates/rynas-ui/src/pages/shares.rs)
const TAB = { general: 0, permissions: 3 };

const editor = (share, tab) => navigate(`/shares?edit=${encodeURIComponent(share.name)}&tab=${TAB[tab]}`);

/** The permissions of a shared folder: the Permissions tab of its editor. */
export function sharePermissions(share) {
  editor(share, "permissions");
}

/** The description of a shared folder: the General tab of its editor. */
export function shareDescription(share) {
  editor(share, "general");
}
