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

// Forms: every mutating POST carries the panel's `X-Ka-Agent: 1` header (the
// server refuses POSTs without it — the second CSRF lock beside
// SameSite=Strict). Plain HTML forms cannot send headers, so their submits are
// intercepted and replayed through fetch; the response replaces the document.
(function () {
  "use strict";
  document.addEventListener("submit", function (ev) {
    var form = ev.target;
    if (!form || form.tagName !== "FORM") return;
    var method = (form.getAttribute("method") || "get").toLowerCase();
    if (method !== "post") return;
    ev.preventDefault();
    var data = new FormData(form);
    var action = form.getAttribute("action") || location.pathname;
    fetch(action, {
      method: "POST",
      headers: { "X-Ka-Agent": "1" },
      body: new URLSearchParams(data),
      credentials: "same-origin",
      redirect: "follow",
    })
      .then(function (r) {
        if (r.status === 401) {
          location.href = "/login";
          return null;
        }
        return r.text();
      })
      .then(function (html) {
        if (html === null) return;
        document.open();
        document.write(html);
        document.close();
      })
      .catch(function (e) {
        alert("request failed: " + e);
      });
  });
})();
