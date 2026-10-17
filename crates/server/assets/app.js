/* auctimogger-tracker UI enhancement layer.
   Everything works without this file (server-rendered HTML is complete);
   the script adds sorting, completions, the interactive chart, target
   switching and — in open-mutation mode — watchlist/fetch actions.
   No dependencies, no build step. */
"use strict";

(function () {
  // ------------------------------------------------------------- helpers
  function $(sel, root) { return (root || document).querySelector(sel); }
  function $all(sel, root) { return Array.prototype.slice.call((root || document).querySelectorAll(sel)); }

  function copperToText(c) {
    var gold = Math.floor(c / 10000);
    var silver = Math.floor((c % 10000) / 100);
    var rem = c % 100;
    if (gold > 0) return gold + "g " + silver + "s " + rem + "c";
    if (silver > 0) return silver + "s " + rem + "c";
    return rem + "c";
  }

  // -------------------------------------------------------- table sorting
  $all("th[data-sortable]").forEach(function (th) {
    th.addEventListener("click", function () {
      var table = th.closest("table");
      if (!table) return;
      var tbody = table.tBodies[0];
      if (!tbody) return;
      var index = Array.prototype.indexOf.call(th.parentElement.children, th);
      var current = th.getAttribute("aria-sort");
      var ascending = current !== "ascending";
      // Clear every other sort marker in this table.
      $all("th[data-sortable]", table).forEach(function (other) {
        if (other !== th) other.removeAttribute("aria-sort");
      });
      th.setAttribute("aria-sort", ascending ? "ascending" : "descending");
      var rows = $all("tr", tbody);
      rows.sort(function (a, b) {
        var av = a.children[index], bv = b.children[index];
        if (!av || !bv) return 0;
        var aVal = av.hasAttribute("data-v") ? parseFloat(av.getAttribute("data-v")) : NaN;
        var bVal = bv.hasAttribute("data-v") ? parseFloat(bv.getAttribute("data-v")) : NaN;
        var cmp;
        if (!isNaN(aVal) && !isNaN(bVal)) {
          cmp = aVal - bVal;
        } else {
          cmp = (av.textContent || "").trim().localeCompare((bv.textContent || "").trim());
        }
        return ascending ? cmp : -cmp;
      });
      rows.forEach(function (row) { tbody.appendChild(row); });
    });
  });

  // ------------------------------------------------------ target switcher
  $all("select[data-autosubmit]").forEach(function (select) {
    select.addEventListener("change", function () { select.form.submit(); });
  });

  // ---------------------------------------------------------- completions
  var box = $("[data-search-endpoint]");
  if (box) {
    var completions = document.createElement("div");
    completions.className = "completions";
    var host = box.closest(".searchbox") || box.parentElement;
    if (host) host.appendChild(completions);
    var target = box.getAttribute("data-search-target") || "";
    var timer = null;
    var active = -1;

    function clear() { completions.textContent = ""; active = -1; }

    function hitUrl(hit) {
      if (hit.type === "pet") {
        return "/items/" + encodeURIComponent(target) + "/pet/" + hit.speciesID;
      }
      return "/items/" + encodeURIComponent(target) + "/" + hit.itemID;
    }

    function render(hits) {
      clear();
      hits.forEach(function (hit) {
        var row = document.createElement("div");
        row.className = "hit";
        var link = document.createElement("a");
        link.href = hitUrl(hit);
        link.textContent = hit.name;
        var meta = document.createElement("span");
        meta.className = "meta";
        var bits = [];
        if (hit.type === "pet") bits.push("pet");
        if (hit.itemClass) bits.push(hit.itemClass);
        bits.push(String(hit.type === "pet" ? hit.speciesID : hit.itemID));
        meta.textContent = bits.join(" · ");
        row.appendChild(link);
        row.appendChild(meta);
        completions.appendChild(row);
      });
    }

    function mark() {
      var hits = $all(".hit", completions);
      hits.forEach(function (el, i) { el.classList.toggle("active", i === active); });
    }

    box.addEventListener("input", function () {
      clearTimeout(timer);
      var value = box.value.trim();
      if (value.length < 2) { clear(); return; }
      timer = setTimeout(function () {
        fetch(box.getAttribute("data-search-endpoint") + "?q=" + encodeURIComponent(value) + "&limit=12")
          .then(function (r) { return r.json(); })
          .then(function (json) { render(json.hits || []); })
          .catch(function () { clear(); });
      }, 220);
    });

    box.addEventListener("keydown", function (event) {
      var count = $all(".hit", completions).length;
      if (!count) return;
      if (event.key === "ArrowDown") { active = (active + 1) % count; mark(); event.preventDefault(); }
      else if (event.key === "ArrowUp") { active = (active - 1 + count) % count; mark(); event.preventDefault(); }
      else if (event.key === "Enter" && active >= 0) {
        var link = $(".hit.active a", completions);
        if (link) { window.location.href = link.href; event.preventDefault(); }
      } else if (event.key === "Escape") { clear(); }
    });

    document.addEventListener("click", function (event) {
      if (!completions.contains(event.target) && event.target !== box) clear();
    });
  }

  // -------------------------------------------------------- history chart
  var island = $("#chart-data");
  var mount = $("#chart");
  if (island && mount) {
    try {
      var data = JSON.parse(island.textContent);
      if (data && data.days && data.days.length >= 2) {
        renderChart(mount, data);
      }
    } catch (_error) {
      /* keep the server-rendered fallback */
    }
  }

  function renderChart(mount, data) {
    var W = 800, H = 240, PAD_L = 8, PAD_R = 8, PAD_T = 14, PAD_B = 26;
    var days = data.days;
    var low = Infinity, high = -Infinity;
    days.forEach(function (d) {
      [d.p05, d.median, d.p75].forEach(function (v) {
        if (v < low) low = v;
        if (v > high) high = v;
      });
    });
    if (low === high) { low -= 1; high += 1; }
    var span = high - low;
    var stepX = (W - PAD_L - PAD_R) / (days.length - 1);
    function x(i) { return PAD_L + i * stepX; }
    function y(v) { return PAD_T + (1 - (v - low) / span) * (H - PAD_T - PAD_B); }

    var NS = "http://www.w3.org/2000/svg";
    function el(tag, attrs) {
      var node = document.createElementNS(NS, tag);
      Object.keys(attrs || {}).forEach(function (k) { node.setAttribute(k, attrs[k]); });
      return node;
    }

    var svg = el("svg", { viewBox: "0 0 " + W + " " + H, role: "img", "aria-label": "price history band chart" });

    // p05-p75 typical-range band.
    var bandPoints = [];
    days.forEach(function (d, i) { bandPoints.push(x(i) + "," + y(d.p05)); });
    for (var i = days.length - 1; i >= 0; i -= 1) {
      bandPoints.push(x(i) + "," + y(days[i].p75));
    }
    svg.appendChild(el("polygon", {
      points: bandPoints.join(" "),
      fill: "rgba(90,162,255,0.16)",
      stroke: "none"
    }));

    // Median line.
    var linePoints = days.map(function (d, i) { return x(i) + "," + y(d.median); });
    svg.appendChild(el("polyline", {
      points: linePoints.join(" "),
      fill: "none",
      stroke: "#5aa2ff",
      "stroke-width": "2",
      "stroke-linejoin": "round",
      "stroke-linecap": "round"
    }));

    // Min/max axis annotations.
    function label(text, xPos, yPos, anchor) {
      var t = el("text", {
        x: xPos, y: yPos, fill: "#8b94a7", "font-size": "11",
        "text-anchor": anchor || "start", "font-family": "inherit"
      });
      t.textContent = text;
      svg.appendChild(t);
    }
    label(copperToText(high), PAD_L, PAD_T - 3);
    label(copperToText(low), PAD_L, H - 8);
    label(days[days.length - 1].label, W - PAD_R, H - 8, "end");

    // Hover guide + crosshair.
    var hover = el("line", { x1: 0, x2: 0, y1: PAD_T, y2: H - PAD_B, stroke: "#5c6577", "stroke-dasharray": "3,3", visibility: "hidden" });
    var dot = el("circle", { r: 3.5, fill: "#5aa2ff", visibility: "hidden" });
    svg.appendChild(hover);
    svg.appendChild(dot);

    var tip = document.createElement("div");
    tip.className = "chart-hover";
    var wrap = document.createElement("div");
    wrap.className = "chart-wrap";
    mount.textContent = "";
    wrap.appendChild(svg);
    wrap.appendChild(tip);
    mount.appendChild(wrap);

    svg.addEventListener("mousemove", function (event) {
      var rect = svg.getBoundingClientRect();
      var relX = (event.clientX - rect.left) / rect.width * W;
      var idx = Math.round((relX - PAD_L) / stepX);
      if (idx < 0) idx = 0;
      if (idx > days.length - 1) idx = days.length - 1;
      var d = days[idx];
      hover.setAttribute("x1", x(idx));
      hover.setAttribute("x2", x(idx));
      hover.setAttribute("visibility", "visible");
      dot.setAttribute("cx", x(idx));
      dot.setAttribute("cy", y(d.median));
      dot.setAttribute("visibility", "visible");
      var bits = [d.label, "median " + copperToText(d.median)];
      if (typeof d.qty === "number" && d.qty > 0) bits.push(d.qty + " qty");
      if (typeof d.n === "number" && d.n > 0) bits.push(d.n + " samples");
      tip.textContent = bits.join("  ·  ");
      var pxRatio = x(idx) / W;
      tip.style.left = Math.min(Math.max(pxRatio * rect.width - tip.offsetWidth / 2, 0), rect.width - tip.offsetWidth) + "px";
      tip.style.top = "0px";
    });
    svg.addEventListener("mouseleave", function () {
      hover.setAttribute("visibility", "hidden");
      dot.setAttribute("visibility", "hidden");
      tip.textContent = "";
    });
  }

  // ------------------------------------------------------ mutation buttons
  // Rendered only in open-mutation mode (no API token configured); when a
  // token is required these elements are absent from the page entirely.
  $all("[data-watch-add]").forEach(function (button) {
    button.addEventListener("click", function () {
      act(button, "/api/watchlist", "POST", button.getAttribute("data-watch-add"));
    });
  });
  $all("[data-watch-remove]").forEach(function (button) {
    button.addEventListener("click", function () {
      act(button, "/api/watchlist", "DELETE", button.getAttribute("data-watch-remove"));
    });
  });
  $all("[data-fetch-now]").forEach(function (button) {
    button.addEventListener("click", function () {
      // The JSON body is required (CSRF hygiene: simple cross-site forms
      // cannot send application/json). `{}` is the documented empty body.
      act(button, "/api/fetch?target=" + encodeURIComponent(button.getAttribute("data-fetch-now")), "POST", "{}");
    });
  });

  function act(button, url, method, body) {
    if (button.disabled) return;
    button.disabled = true;
    var options = { method: method, headers: { "Content-Type": "application/json" } };
    if (body) options.body = body;
    fetch(url, options)
      .then(function (response) {
        if (!response.ok) {
          return response.text().then(function (text) {
            throw new Error(text || ("HTTP " + response.status));
          });
        }
        return response;
      })
      .then(function () { window.location.reload(); })
      .catch(function (error) {
        window.alert("request failed: " + error.message);
        button.disabled = false;
      });
  }

  // ------------------------------------------------------------ CSV export
  $all("[data-csv]").forEach(function (button) {
    button.addEventListener("click", function () {
      var table = document.getElementById(button.getAttribute("data-csv"));
      if (!table) return;
      var lines = [];
      $all("tr", table).forEach(function (row) {
        if (row.closest("tfoot")) return;
        var cells = $all("th,td", row).map(function (cell) {
          var text = (cell.getAttribute("data-export") || cell.textContent).trim();
          return '"' + text.replace(/"/g, '""') + '"';
        });
        lines.push(cells.join(","));
      });
      var blob = new Blob([lines.join("\r\n")], { type: "text/csv;charset=utf-8" });
      var link = document.createElement("a");
      link.href = URL.createObjectURL(blob);
      link.download = (button.getAttribute("data-csv") || "export") + ".csv";
      document.body.appendChild(link);
      link.click();
      link.remove();
    });
  });

  // ---------------------------------------------------------- dashboard refresh
  // The dashboard only reloads while nothing is in the way: not while the
  // user is typing, and never more often than the server-suggested cadence.
  if ($("[data-autoreload-note]")) {
    setInterval(function () {
      var active = document.activeElement;
      var typing = active && (active.tagName === "INPUT" || active.tagName === "SELECT" || active.tagName === "TEXTAREA");
      if (!typing && document.visibilityState === "visible") window.location.reload();
    }, 30000);
  }
})();
