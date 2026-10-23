// Apply the stored theme before first paint to avoid a flash. The settings live under the name of
// the current localStorage layout (src/lib/local-storage.ts): `gwar.storage-version` names it.
      try {
        var v = /^([1-9][0-9]{0,8})\.([0-9a-f]{16})$/.exec(localStorage.getItem("gwar.storage-version") || "");
        var s = JSON.parse(localStorage.getItem(v ? "gwar.v" + v[1] + "." + v[2] + ".settings" : "gwar.settings") || "{}").state || {};
        var t = s.theme || "system";
        if (t === "system") t = matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
        document.documentElement.dataset.theme = t;
      } catch {
        document.documentElement.dataset.theme = "dark";
      }
