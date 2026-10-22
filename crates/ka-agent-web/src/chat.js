// Ka Agent web chat: one vanilla file, no framework, no build step.
// Every user-derived string goes through textContent — never innerHTML.

(function () {
  "use strict";

  var log = document.getElementById("log");
  var form = document.getElementById("send");
  var input = document.getElementById("text");
  if (!log || !form || !input) return;

  function bubble(cls) {
    var card = document.createElement("div");
    card.className = "bubble " + (cls || "");
    log.appendChild(card);
    return card;
  }

  function line(parent, text, cls) {
    var div = document.createElement("div");
    if (cls) div.className = cls;
    div.textContent = text;
    parent.appendChild(div);
    return div;
  }

  // The same Markdown subset the other surfaces render (bold, `code`,
  // [links](http…), bullets, headings, fences) — built from nodes, never
  // innerHTML, so nothing is ever interpreted as markup.
  function md(parent, text) {
    var lines = String(text).split("\n");
    var i = 0;
    while (i < lines.length) {
      var l = lines[i];
      if (/^```/.test(l.trim())) {
        var pre = document.createElement("pre");
        var code = document.createElement("code");
        pre.appendChild(code);
        i++;
        while (i < lines.length && lines[i].trim() !== "```") {
          code.appendChild(document.createTextNode(lines[i]));
          code.appendChild(document.createTextNode("\n"));
          i++;
        }
        i++; // closing fence
        parent.appendChild(pre);
        continue;
      }
      parent.appendChild(mdLine(l));
      i++;
    }
  }

  function mdLine(l) {
    var h = /^(#{1,6}) (.*)$/.exec(l);
    if (h) {
      var b = document.createElement("b");
      mdInline(b, h[2]);
      return b;
    }
    var m = /^(\s*)[-*+] (.*)$/.exec(l);
    if (m) {
      var out = document.createElement("span");
      out.appendChild(document.createTextNode(m[1] + "• "));
      mdInline(out, m[2]);
      out.appendChild(document.createElement("br"));
      return out;
    }
    var p = document.createElement("span");
    mdInline(p, l);
    p.appendChild(document.createElement("br"));
    return p;
  }

  function mdInline(parent, s) {
    var rest = s;
    for (;;) {
      var code = /`([^`]+)`/.exec(rest);
      var bold = /\*\*(.+?)\*\*|__(.+?)__/.exec(rest);
      var link = /\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/.exec(rest);
      var first = null;
      if (code) first = code;
      if (bold && (!first || bold.index < first.index)) first = bold;
      if (link && (!first || link.index < first.index)) first = link;
      if (!first) break;
      parent.appendChild(document.createTextNode(rest.slice(0, first.index)));
      if (first === code) {
        var c = document.createElement("code");
        c.textContent = code[1];
        parent.appendChild(c);
        rest = rest.slice(first.index + first[0].length);
      } else if (first === link) {
        var a = document.createElement("a");
        a.href = link[2];
        a.target = "_blank";
        a.rel = "noopener noreferrer";
        a.textContent = link[1];
        parent.appendChild(a);
        rest = rest.slice(first.index + first[0].length);
      } else {
        var b = document.createElement("b");
        mdInline(b, bold[1] || bold[2]);
        parent.appendChild(b);
        rest = rest.slice(first.index + first[0].length);
      }
    }
    parent.appendChild(document.createTextNode(rest));
  }

  var current = null; // the in-flight assistant bubble
  var currentText = ""; // its raw text, re-rendered when the turn ends
  var notes = [];

  function assistant() {
    if (!current) {
      current = bubble("assistant");
      currentText = "";
    }
    return current;
  }

  function flushNotes() {
    if (!current || !notes.length) return;
    var p = document.createElement("div");
    p.className = "muted";
    p.textContent = notes.join("\n");
    current.appendChild(p);
    notes = [];
  }

  function finish() {
    if (current && currentText) {
      // Streamed raw while typing; rendered when the turn settles.
      while (current.firstChild) current.removeChild(current.firstChild);
      md(current, currentText);
    }
    flushNotes();
    current = null;
    currentText = "";
  }

  function handleEvent(ev) {
    switch (ev.ev) {
      case "inbound":
        finish();
        if (ev.source !== "web") {
          line(bubble("user"), "you (" + ev.source + "): " + ev.text);
        }
        break;
      case "turn_started":
        finish();
        current = bubble("assistant");
        line(current, "…", "muted").className = "pending muted";
        break;

      case "delta":
        if (!current) current = bubble("assistant");
        var pending = current.querySelector(".pending");
        if (pending) pending.remove();
        currentText += ev.text;
        current.appendChild(document.createTextNode(ev.text));
        break;
      case "note":
        notes.push(ev.text);
        var p = current && current.querySelector(".pending .muted");
        if (p) p.textContent = notes.join("\n");
        break;
      case "notice":
        finish();
        line(bubble("system"), ev.text, "muted").style.textAlign = "center";
        break;
      case "approval":
        finish();
        current = bubble("approval");
        line(current, "approval " + ev.id);
        line(current, ev.question);
        var actions = document.createElement("div");
        var allow = document.createElement("button");
        allow.textContent = "Allow";
        var deny = document.createElement("button");
        deny.textContent = "Deny";
        allow.addEventListener("click", function () {
          decide(ev.id, "y", actions);
        });
        deny.addEventListener("click", function () {
          decide(ev.id, "n", actions);
        });
        actions.appendChild(allow);
        actions.appendChild(deny);
        current.appendChild(actions);
        break;
      case "approval_settled":
        finish();
        var cards = log.querySelectorAll(".approval");
        for (var i = cards.length - 1; i >= 0; i--) {
          var buttons = cards[i].querySelectorAll("button");
          for (var j = 0; j < buttons.length; j++) buttons[j].disabled = true;
          var label = cards[i].querySelector(".settled");
          if (!label) {
            label = line(cards[i], "approval " + ev.id + " → " + ev.status, "muted");
            label.className = "settled muted";
          }
        }
        break;
      case "turn_finished":
        finish();
        line(
          bubble("system"),
          "· " + (ev.stop || "") + " · $" + Number(ev.cost_usd || 0).toFixed(4) + " · " + (ev.tokens || 0) + " tokens",
          "muted"
        );
        break;
      case "error":
        finish();
        line(bubble("system"), "! [" + ev["class"] + "] " + ev.message, "error-line").style.color = "#b00";
        break;
      default:
        break;
    }
    window.scrollTo(0, document.body.scrollHeight);
  }

  function decide(id, decision, actions) {
    var body = "id=" + encodeURIComponent(id) + "&decision=" + decision;
    fetch("/approvals/decide", {
      method: "POST",
      headers: { "X-Ka-Agent": "1", "Content-Type": "application/x-www-form-urlencoded" },
      body: body,
    }).catch(function () {});
    var buttons = actions.querySelectorAll("button");
    for (var i = 0; i < buttons.length; i++) buttons[i].disabled = true;
  }

  function send() {
    var v = input.value.trim();
    if (!v) return;
    input.value = "";
    line(bubble("user"), v);
    current = null;
    var body = "text=" + encodeURIComponent(v);
    fetch("/chat/send", {
      method: "POST",
      headers: { "X-Ka-Agent": "1", "Content-Type": "application/x-www-form-urlencoded" },
      body: body,
    })
      .then(function (r) {
        return r.json();
      })
      .then(function (data) {
        if (data && data.reply) {
          finish();
          line(bubble("system"), data.reply, "muted");
        }
      })
      .catch(function () {
        finish();
        line(bubble("system"), "! could not send", "error-line");
      });
  }

  form.addEventListener("submit", function (e) {
    e.preventDefault();
    send();
  });
  input.addEventListener("keydown", function (e) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  });

  try {
    var source = new EventSource("/events");
    source.onmessage = function (m) {
      try {
        handleEvent(JSON.parse(m.data));
      } catch (_) {}
    };
  } catch (_) {}
})();
