// Build-time patch for the bundled jellyfin-web client, applied in the Docker
// ui-build stage (see Dockerfile). NON-FATAL: if the upstream source has moved
// on a future UI bump, it logs a warning and leaves the client untouched.
//
// Why: Jellyswarrm applies the user's library order server-side (it sorts the
// /UserViews response from OrderedViews). But the web client caches /UserViews in
// React Query and the home grid does not refetch it when the order is changed, so
// the new order only shows after a full page reload (the drawer, a live query
// observer, refreshes on its own). This patch invalidates the cached views and
// nudges the home tab to reload right after a Home-layout save.

import { readFileSync, writeFileSync, existsSync } from 'node:fs';

const FILE = 'src/components/homeScreenSettings/homeScreenSettings.js';

if (!existsSync(FILE)) {
    console.warn(`[jellyswarrm-patch] ${FILE} not found; skipping`);
    process.exit(0);
}

let src = readFileSync(FILE, 'utf8');

if (src.includes('jellyswarrm home refresh')) {
    console.log('[jellyswarrm-patch] home-refresh patch already present; skipping');
    process.exit(0);
}

const anchor = "            Events.trigger(instance, 'saved');\n        }, () => {";

if (!src.includes(anchor)) {
    console.warn('[jellyswarrm-patch] anchor not found (UI changed?); skipping home-refresh patch');
    process.exit(0);
}

const replacement =
    "            Events.trigger(instance, 'saved');\n\n" +
    "            // jellyswarrm home refresh: the proxy applies library order via\n" +
    "            // /UserViews, but the client caches it and the home grid doesn't\n" +
    "            // refetch on an OrderedViews change. Invalidate the cached views\n" +
    "            // and nudge the home tab to reload so the new order shows at once.\n" +
    "            try {\n" +
    "                queryClient.invalidateQueries({ queryKey: ['User', userId, 'Views'] });\n" +
    "                document.querySelectorAll('.sections').forEach(function (el) {\n" +
    "                    el.dispatchEvent(new CustomEvent('settingschange'));\n" +
    "                });\n" +
    "            } catch (e) {\n" +
    "                console.error('[jellyswarrm] home refresh failed', e);\n" +
    "            }\n" +
    "        }, () => {";

src = src.replace(anchor, replacement);
writeFileSync(FILE, src);
console.log('[jellyswarrm-patch] applied home-refresh patch to homeScreenSettings.js');
