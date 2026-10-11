// Apply the stored theme before first paint to avoid a flash.
      try {
        var s = JSON.parse(localStorage.getItem("vc.settings") || "{}").state || {};
        var t = s.theme || "system";
        if (t === "system") t = matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
        document.documentElement.dataset.theme = t;
      } catch {
        document.documentElement.dataset.theme = "dark";
      }
