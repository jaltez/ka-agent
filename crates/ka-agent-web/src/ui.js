// Ka Agent web UI: theme preference. No framework, no external assets.
// "system" follows prefers-color-scheme; light/dark pin the palette through
// `data-theme` (the stylesheet keeps both token sets).
(function () {
  "use strict";
  var KEY = "ka-theme";
  var order = ["system", "light", "dark"];
  var root = document.documentElement;
  var button = document.getElementById("theme-toggle");
  var current = "system";
  try {
    current = localStorage.getItem(KEY) || "system";
  } catch (_) {}
  if (order.indexOf(current) < 0) current = "system";

  function apply(theme) {
    if (theme === "light" || theme === "dark") {
      root.setAttribute("data-theme", theme);
    } else {
      root.removeAttribute("data-theme");
    }
    if (button) button.textContent = "theme: " + theme;
  }

  apply(current);
  if (button) {
    button.addEventListener("click", function () {
      current = order[(order.indexOf(current) + 1) % order.length];
      try {
        localStorage.setItem(KEY, current);
      } catch (_) {}
      apply(current);
    });
  }
})();
