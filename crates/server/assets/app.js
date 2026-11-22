/* auctimogger-tracker UI enhancement layer.
   Everything works without this file (server-rendered HTML is complete);
   the script adds sorting, completions, the interactive chart, target
   switching and mutation actions (live in open mode; on protected
   instances the API key button unlocks them for the tab session).
   No dependencies, no build step. */
"use strict";

(function () {
  // Signals CSS that scripting is active: the pure-CSS tooltip bubbles are
  // disabled (they get clipped by .scroll-x tables) in favor of the floating
  // layer below.
  document.documentElement.classList.add("js");

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
    // Band top is p95 when the island carries it; p75 keeps old islands alive.
    function bandTop(d) { return typeof d.p95 === "number" ? d.p95 : d.p75; }
    var low = Infinity, high = -Infinity;
    days.forEach(function (d) {
      [d.p05, d.median, bandTop(d), d.vwap].forEach(function (v) {
        if (typeof v !== "number") return;
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

    // p05-p95 typical-range band.
    var bandPoints = [];
    days.forEach(function (d, i) { bandPoints.push(x(i) + "," + y(d.p05)); });
    for (var i = days.length - 1; i >= 0; i -= 1) {
      bandPoints.push(x(i) + "," + y(bandTop(days[i])));
    }
    svg.appendChild(el("polygon", {
      points: bandPoints.join(" "),
      fill: "rgba(90,162,255,0.16)",
      stroke: "none"
    }));

    // VWAP line: dashed gold, only when every day carries a numeric vwap.
    if (days.every(function (d) { return typeof d.vwap === "number"; })) {
      svg.appendChild(el("polyline", {
        points: days.map(function (d, i) { return x(i) + "," + y(d.vwap); }).join(" "),
        fill: "none",
        stroke: "#f0b429",
        "stroke-width": "1.5",
        "stroke-dasharray": "5,4",
        "stroke-linejoin": "round",
        "stroke-linecap": "round"
      }));
    }

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
      if (typeof d.p95 === "number") bits.push("p95 " + copperToText(d.p95));
      if (typeof d.vwap === "number") bits.push("vwap " + copperToText(d.vwap));
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

  // --------------------------------------------------------- when to buy
  // The server renders the when-to-buy section in UTC. With JS the section
  // re-buckets everything into the VIEWER's local timezone: weekday stats
  // re-aggregate from the raw per-day rows, hour cells re-key by the local
  // hour of each (UTC day, UTC hour) pair — a 23:00 UTC sample belongs to
  // the next local day where it belongs.
  (function () {
    var island = $("#when-data");
    if (!island) return;
    var data;
    try { data = JSON.parse(island.textContent); } catch (_error) { return; }
    if (!data) return;

    var WEEKDAYS = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
    var WEEKDAYS_FULL =
      ["sunday", "monday", "tuesday", "wednesday", "thursday", "friday", "saturday"];
    // Monday-first display order (weekday indexing is 0 = Sunday).
    var DISPLAY_ORDER = [1, 2, 3, 4, 5, 6, 0];

    function medianOf(values) {
      if (!values.length) return null;
      var sorted = values.slice().sort(function (a, b) { return a - b; });
      var mid = Math.ceil(sorted.length / 2);
      if (sorted.length % 2) return sorted[mid - 1];
      return Math.floor((sorted[mid - 1] + sorted[mid]) / 2);
    }
    function pad2(value) { return (value < 10 ? "0" : "") + value; }
    function pctText(value, base) {
      return base > 0 ? (value / base * 100 - 100).toFixed(1) + "% vs typical" : "—";
    }

    var NS = "http://www.w3.org/2000/svg";
    function svgEl(tag, attrs) {
      var node = document.createElementNS(NS, tag);
      for (var key in attrs) node.setAttribute(key, attrs[key]);
      return node;
    }

    // ---- weekday view: re-aggregate the raw day rows by LOCAL weekday.
    var stats = null, weekMedian = null, weekdayBest = null;
    var weekdayMount = $("#when-weekday");
    if (weekdayMount && data.weekdayDays && data.weekdayDays.length) {
      var byWeekday = {};
      data.weekdayDays.forEach(function (row) {
        var weekday = new Date(row.day * 86400000).getDay();
        if (!byWeekday[weekday]) byWeekday[weekday] = { medians: [], p05s: [] };
        byWeekday[weekday].medians.push(row.median);
        byWeekday[weekday].p05s.push(row.p05);
      });
      stats = {};
      Object.keys(byWeekday).forEach(function (weekday) {
        var group = byWeekday[weekday];
        if (group.medians.length < 2) return; // one day is not a pattern
        stats[weekday] = {
          median: medianOf(group.medians),
          p05: medianOf(group.p05s),
          days: group.medians.length
        };
      });
      var keys = Object.keys(stats);
      if (keys.length) {
        keys.forEach(function (weekday) {
          if (weekdayBest === null || stats[weekday].median < stats[weekdayBest].median) {
            weekdayBest = Number(weekday);
          }
        });
        weekMedian = medianOf(keys.map(function (weekday) { return stats[weekday].median; }));

        var W = 7 * 40 + 8, H = 116;
        var max = Math.max.apply(null, keys.map(function (w) { return stats[w].median; }));
        var svg = svgEl("svg", {
          viewBox: "0 0 " + W + " " + H, role: "img",
          "aria-label": "price by weekday (local time)", style: "max-width:" + W + "px"
        });
        DISPLAY_ORDER.forEach(function (weekday, slot) {
          var x = slot * 40 + 8;
          var stat = stats[weekday];
          if (stat) {
            var bar = Math.max(stat.median / max * 72, 2);
            var floor = Math.max(stat.p05 / max * 72, 2);
            var g = svgEl("g", {
              class: "tip", tabindex: "0",
              "data-tip": WEEKDAYS_FULL[weekday] + " · median " + copperToText(stat.median) +
                " · floor " + copperToText(stat.p05) + " · " + stat.days + " days"
            });
            g.appendChild(svgEl("rect", { x: x, y: 86 - bar, width: 26, height: bar, rx: 3, fill: "#5aa2ff", opacity: 0.75 }));
            g.appendChild(svgEl("rect", { x: x, y: 86 - floor, width: 26, height: 2, fill: "#1b2233" }));
            svg.appendChild(g);
          } else {
            var dash = svgEl("text", { x: x + 13, y: 86, "text-anchor": "middle", class: "muted" });
            dash.textContent = "—";
            svg.appendChild(dash);
          }
          var label = svgEl("text", { x: x + 13, y: H, "text-anchor": "middle", class: "muted" });
          label.textContent = WEEKDAYS[weekday];
          svg.appendChild(label);
        });
        weekdayMount.textContent = "";
        var caption = document.createElement("p");
        caption.className = "muted";
        caption.textContent = "median by weekday · notch = p05 floor · your local weekdays";
        weekdayMount.appendChild(caption);
        weekdayMount.appendChild(svg);
      }
    }

    // ---- intraday view: re-key the (UTC day, UTC hour) cells by local hour.
    var hourTypicals = null, hourMedian = null, hourBest = null;
    var intradayMount = $("#when-intraday");
    if (intradayMount && data.days && data.days.length) {
      var byHour = {};      // local hour -> typicals as % of the day's median
      var floorByHour = {}; // local hour -> floors as %
      var perDay = [];      // faint single-day lines
      data.days.forEach(function (dayEntry) {
        if (!dayEntry.hours || !dayEntry.hours.length) return;
        var dayMedian = dayEntry.median;
        if (!(typeof dayMedian === "number" && dayMedian > 0)) return;
        var points = [];
        dayEntry.hours.forEach(function (cell) {
          var local = new Date((dayEntry.day * 86400 + cell.h * 3600) * 1000);
          var hour = local.getHours();
          var typical = cell.typical / dayMedian * 100;
          var floor = cell.floor / dayMedian * 100;
          if (!byHour[hour]) byHour[hour] = [];
          if (!floorByHour[hour]) floorByHour[hour] = [];
          byHour[hour].push(typical);
          floorByHour[hour].push(floor);
          points.push({ hour: hour, pct: typical });
        });
        if (points.length >= 2) {
          // Draw the day's path in LOCAL-hour order: cells arrive in UTC-hour
          // order, and for any non-UTC viewer the local-hour sequence wraps
          // once through midnight — unsorted, that wrap draws a long segment
          // back across the whole chart.
          points.sort(function (a, b) { return a.hour - b.hour; });
          perDay.push({ label: new Date(dayEntry.day * 86400000).toLocaleDateString(), points: points });
        }
      });
      var hourKeys = Object.keys(byHour).map(Number).sort(function (a, b) { return a - b; });
      if (hourKeys.length) {
        hourTypicals = hourKeys.map(function (hour) {
          return { hour: hour, typical: medianOf(byHour[hour]), floor: Math.min.apply(null, floorByHour[hour]) };
        });
        hourMedian = medianOf(hourTypicals.map(function (entry) { return entry.typical; }));
        hourTypicals.forEach(function (entry, index) {
          if (hourBest === null || entry.typical < hourTypicals[hourBest].typical) hourBest = index;
        });

        var W = 560, H = 130, PAD_T = 14, PAD_B = 24;
        var values = [];
        hourTypicals.forEach(function (entry) { values.push(entry.typical, entry.floor); });
        perDay.forEach(function (line) {
          line.points.forEach(function (point) { values.push(point.pct); });
        });
        var low = Math.floor(Math.min.apply(null, values));
        var high = Math.ceil(Math.max.apply(null, values));
        var span = Math.max(high - low, 1);
        function x(hour) { return 14 + hour / 23 * (W - 28); }
        function y(pct) { return PAD_T + (1 - (pct - low) / span) * (H - PAD_T - PAD_B); }
        var svg = svgEl("svg", {
          viewBox: "0 0 " + W + " " + H, role: "img",
          "aria-label": "price by hour of day (local time)", style: "max-width:" + W + "px"
        });
        perDay.forEach(function (line) {
          var points = line.points.map(function (point) { return x(point.hour) + "," + y(point.pct); }).join(" ");
          svg.appendChild(svgEl("polyline", { points: points, fill: "none", stroke: "#5aa2ff", "stroke-width": 1, opacity: 0.18 }));
        });
        svg.appendChild(svgEl("polyline", {
          points: hourTypicals.map(function (entry) { return x(entry.hour) + "," + y(entry.floor); }).join(" "),
          fill: "none", stroke: "#7f8ba3", "stroke-width": 1, "stroke-dasharray": "3,3"
        }));
        svg.appendChild(svgEl("polyline", {
          points: hourTypicals.map(function (entry) { return x(entry.hour) + "," + y(entry.typical); }).join(" "),
          fill: "none", stroke: "#5aa2ff", "stroke-width": 2
        }));
        [0, 6, 12, 18, 23].forEach(function (hour) {
          var label = svgEl("text", {
            x: x(hour), y: H - 6,
            "text-anchor": hour === 0 ? "start" : hour === 23 ? "end" : "middle",
            class: "muted"
          });
          label.textContent = pad2(hour);
          svg.appendChild(label);
        });
        var hi = svgEl("text", { x: 14, y: PAD_T, class: "muted" });
        hi.textContent = high + "%";
        svg.appendChild(hi);
        intradayMount.textContent = "";
        var caption = document.createElement("p");
        caption.className = "muted";
        caption.textContent = "typical ask by hour of your local day · % of that day's median · faint = single days · dashed = floor";
        intradayMount.appendChild(caption);
        intradayMount.appendChild(svg);
      }
    }

    // ---- verdict line, recomputed for local time.
    var verdict = $("#when-verdict");
    if (verdict) {
      var parts = [];
      if (stats && weekdayBest !== null && weekMedian !== null) {
        parts.push("cheapest weekday: " + WEEKDAYS_FULL[weekdayBest] + " (" +
          pctText(stats[weekdayBest].median, weekMedian) + ")");
      }
      if (hourTypicals && hourBest !== null && hourMedian !== null) {
        parts.push("cheapest hour: " + pad2(hourTypicals[hourBest].hour) + ":00 your time (" +
          pctText(hourTypicals[hourBest].typical, hourMedian) + ")");
      }
      if (parts.length) verdict.textContent = parts.join(" · ");
    }
  })();

  // ------------------------------------------------------ mutation buttons
  // Open instances render these live. Protected instances render them
  // disabled with data-auth; the API key section below enables them once a
  // token is stored for the tab session.
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
  $all("[data-alert-remove]").forEach(function (button) {
    button.addEventListener("click", function () {
      act(button, "/api/alerts", "DELETE", JSON.stringify({ ruleID: Number(button.getAttribute("data-alert-remove")) }));
    });
  });

  // The alerts page rule form: serializes the condition union — pets win
  // when a species id is filled, otherwise the item id is required.
  $all("form[data-alert-form]").forEach(function (form) {
    form.addEventListener("submit", function (event) {
      event.preventDefault();
      var get = function (name) { return form.elements[name].value.trim(); };
      var payload = { target: form.getAttribute("data-target") };
      var species = get("speciesID");
      if (species) {
        payload.item = { type: "pet", speciesID: Number(species), level: Number(get("level") || "1"), qualityID: Number(get("qualityID") || "0"), breedID: Number(get("breedID") || "0") };
      } else {
        payload.item = { type: "item", itemID: Number(get("itemID")) };
      }
      var type = get("type");
      payload.condition = { type: type };
      if (type === "price-below") payload.condition.copper = Number(get("copper"));
      if (type === "below-floor") { payload.condition.span = get("span") || "d7"; payload.condition.percentile = get("percentile") || "p05"; }
      if (type === "sale-rate-above") payload.condition.rate = Number(get("rate"));
      if (type === "days-since-last-sale-above") payload.condition.days = Number(get("days"));
      if (get("note")) payload.note = get("note");
      var button = form.querySelector("button[type=submit]");
      if (button) {
        act(button, "/api/alerts", "POST", JSON.stringify(payload));
      }
    });
  });

  // ---------------------------------------------------------- API key layer
  // Protected instances render mutation controls disabled (data-auth). The
  // key button collects the instance's API token, keeps it in sessionStorage
  // (per tab session: never persisted to disk, never sent anywhere but this
  // origin) and attaches it as the Authorization header on mutations. A
  // rejected (stale) token is cleared and re-asked on the next mutation.
  var TOKEN_KEY = "amt_api_token";
  function token() {
    try { return sessionStorage.getItem(TOKEN_KEY) || ""; } catch (error) { return ""; }
  }
  function saveToken(value) {
    try { sessionStorage.setItem(TOKEN_KEY, value); } catch (error) { /* private mode: keys stay unset */ }
  }
  function forgetToken() {
    try { sessionStorage.removeItem(TOKEN_KEY); } catch (error) { /* same */ }
  }

  var keyButton = null;
  function applyAuthState() {
    var keyed = !!token();
    $all("[data-auth]").forEach(function (control) {
      if (keyed) control.removeAttribute("disabled");
      else control.setAttribute("disabled", "disabled");
    });
    if (keyButton) {
      keyButton.textContent = keyed ? "api key active" : "set api key";
      keyButton.title = keyed
        ? "API key stored for this tab — click to clear it"
        : "mutations need the API token; click to enter it";
      keyButton.classList.toggle("btn-danger", keyed);
    }
  }

  if (document.querySelector("[data-auth]")) {
    keyButton = document.createElement("button");
    keyButton.className = "btn btn-sm key-affordance";
    keyButton.addEventListener("click", function () {
      if (token()) {
        forgetToken();
        applyAuthState();
        return;
      }
      keyDialog("mutations on this instance need the API token", null);
    });
    document.body.appendChild(keyButton);
    applyAuthState();
  }

  var dialog = null;
  function closeKeyDialog() {
    if (dialog) {
      dialog.remove();
      dialog = null;
    }
  }
  function keyDialog(message, onSaved) {
    if (dialog) return;
    dialog = document.createElement("div");
    dialog.className = "key-dialog";
    dialog.innerHTML =
      '<div class="card"><h3>api key</h3>' +
      '<p class="muted">' + message + "</p>" +
      '<input type="password" placeholder="bearer token" aria-label="API token">' +
      '<div class="key-dialog-actions">' +
      '<button class="btn btn-sm" type="button" data-save>save</button>' +
      '<button class="btn btn-sm" type="button" data-cancel>cancel</button></div></div>';
    var input = dialog.querySelector("input");
    function save() {
      var value = input.value.trim();
      closeKeyDialog();
      if (!value) return;
      saveToken(value);
      applyAuthState();
      if (onSaved) onSaved();
    }
    dialog.querySelector("[data-save]").addEventListener("click", save);
    dialog.querySelector("[data-cancel]").addEventListener("click", closeKeyDialog);
    input.addEventListener("keydown", function (event) {
      if (event.key === "Enter") save();
      if (event.key === "Escape") closeKeyDialog();
    });
    document.body.appendChild(dialog);
    input.focus();
  }

  function send(url, method, body, done) {
    var options = { method: method, headers: { "Content-Type": "application/json" } };
    var key = token();
    if (key) options.headers["Authorization"] = "Bearer " + key;
    if (body) options.body = body;
    fetch(url, options)
      .then(function (response) {
        if (response.ok) {
          done(null);
          return;
        }
        return response.text().then(function (text) {
          if (response.status === 401) {
            done(401, text);
            return;
          }
          throw new Error(text || "HTTP " + response.status);
        });
      })
      .catch(function (error) { done(0, error.message); });
  }

  function act(button, url, method, body) {
    if (button.disabled) return;
    button.disabled = true;
    send(url, method, body, function (status, text) {
      if (status === null) {
        window.location.reload();
        return;
      }
      if (status === 401) {
        // Missing or stale key: clear it, ask once, retry the request.
        forgetToken();
        applyAuthState();
        keyDialog("token rejected — enter the API key", function () {
          button.disabled = false;
          act(button, url, method, body);
        });
        return;
      }
      window.alert("request failed: " + (text || "HTTP " + status));
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

  // -------------------------------------------------------------- tooltips
  // One shared floating layer for every [data-tip] element. The CSS-only
  // bubble is clipped inside .scroll-x tables (overflow cuts everything
  // above the header row), so this positions a single fixed node near the
  // hovered/focused element, clamped to the viewport.
  var tip_node = null;
  function tip_show(target) {
    if (!tip_node) {
      tip_node = document.createElement("div");
      tip_node.id = "tooltip";
      tip_node.setAttribute("role", "tooltip");
      document.body.appendChild(tip_node);
    }
    tip_node.textContent = target.getAttribute("data-tip") || "";
    tip_node.style.display = "block";
    var rect = target.getBoundingClientRect();
    var width = tip_node.offsetWidth;
    var height = tip_node.offsetHeight;
    var left = rect.left + rect.width / 2 - width / 2;
    left = Math.max(8, Math.min(left, window.innerWidth - width - 8));
    var top = rect.top - height - 8;
    if (top < 8) top = rect.bottom + 8;
    tip_node.style.left = Math.round(left) + "px";
    tip_node.style.top = Math.round(top) + "px";
  }
  function tip_hide() { if (tip_node) tip_node.style.display = "none"; }
  function tip_target(node) { return node && node.closest ? node.closest("[data-tip]") : null; }
  document.addEventListener("mouseover", function (e) {
    var target = tip_target(e.target);
    if (target) tip_show(target); else tip_hide();
  });
  document.addEventListener("focusin", function (e) {
    var target = tip_target(e.target);
    if (target) tip_show(target);
  });
  document.addEventListener("focusout", tip_hide);
  document.addEventListener("mouseleave", tip_hide);

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
