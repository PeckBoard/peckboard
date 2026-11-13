// Client-side search over the public PeckBoard skills catalog
// (github.com/PeckBoard/skills). Loads index.json from the container's
// data-index-url, filters by text and group, lists versions, fetches a
// version's SKILL.md on expand, and links to "Subscribe in PeckBoard" on
// the instance address the visitor saved (localStorage).
(function () {
  "use strict";

  var root = document.getElementById("skills-search");
  if (!root) return;

  var INDEX_URL =
    root.getAttribute("data-index-url") ||
    "https://raw.githubusercontent.com/PeckBoard/skills/main/index.json";
  var RAW_BASE = INDEX_URL.replace(/index\.json(\?.*)?$/, "");
  var STORAGE_KEY = "peckboard.instanceUrl";
  var NAME_RE = /^[a-z0-9][a-z0-9-]{0,63}$/;
  var VERSION_RE = /^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/;

  var qInput = document.getElementById("skills-q");
  var groupSelect = document.getElementById("skills-group");
  var statusEl = document.getElementById("skills-status");
  var list = document.getElementById("skills-results");
  var instanceForm = document.getElementById("skills-instance-form");
  var instanceInput = document.getElementById("skills-instance-url");
  var instanceStatus = document.getElementById("skills-instance-status");

  var index = { skills: [], groups: [] };
  var bodyCache = {};

  // ── Instance address ───────────────────────────────────────────────

  function readInstance() {
    try {
      return window.localStorage.getItem(STORAGE_KEY) || "";
    } catch (e) {
      return "";
    }
  }

  /** Normalize to scheme://host[:port][/path] without a trailing slash, or null. */
  function normalizeInstance(raw) {
    var v = String(raw || "").trim();
    if (!v) return "";
    if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(v)) v = "https://" + v;
    var u;
    try {
      u = new URL(v);
    } catch (e) {
      return null;
    }
    if (u.protocol !== "https:" && u.protocol !== "http:") return null;
    if (!u.hostname) return null;
    return (u.origin + u.pathname).replace(/\/+$/, "");
  }

  function subscribeHref(base, name) {
    return base + "/settings?tab=skills&subscribe=" + encodeURIComponent(name);
  }

  function showInstance() {
    var base = readInstance();
    instanceInput.value = base;
    instanceStatus.textContent = base ? "Subscribe links open " + base : "";
    var links = list.querySelectorAll("a.skill-subscribe");
    for (var i = 0; i < links.length; i++) updateSubscribeLink(links[i], base);
  }

  function updateSubscribeLink(a, base) {
    var name = a.getAttribute("data-skill");
    if (base) {
      a.href = subscribeHref(base, name);
      a.removeAttribute("data-needs-instance");
      a.title = "Opens Settings → Skills on your instance";
    } else {
      a.href = "#skills-instance-url";
      a.setAttribute("data-needs-instance", "true");
      a.title = "Save your PeckBoard address above first";
    }
  }

  instanceForm.addEventListener("submit", function (ev) {
    ev.preventDefault();
    var norm = normalizeInstance(instanceInput.value);
    if (norm === null) {
      instanceStatus.textContent = "That is not an http(s) address.";
      return;
    }
    try {
      if (norm) window.localStorage.setItem(STORAGE_KEY, norm);
      else window.localStorage.removeItem(STORAGE_KEY);
    } catch (e) {
      /* storage blocked: the link still works for this page view */
    }
    showInstance();
    if (!norm) instanceStatus.textContent = "Address cleared.";
  });

  // ── Rendering ──────────────────────────────────────────────────────

  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
  }

  function groupName(slug) {
    for (var i = 0; i < index.groups.length; i++) {
      if (index.groups[i].slug === slug) return index.groups[i].name || slug;
    }
    return slug;
  }

  function skillGroups(skill) {
    var out = (skill.groups || []).slice();
    for (var i = 0; i < index.groups.length; i++) {
      var g = index.groups[i];
      if ((g.skills || []).indexOf(skill.name) >= 0 && out.indexOf(g.slug) < 0)
        out.push(g.slug);
    }
    return out;
  }

  function stripFrontmatter(text) {
    var t = String(text).replace(/\r\n/g, "\n");
    if (t.indexOf("---\n") !== 0) return t;
    var end = t.indexOf("\n---", 4);
    if (end < 0) return t;
    return t.slice(t.indexOf("\n", end + 1) + 1).replace(/^\n+/, "");
  }

  function loadBody(skill, version, target) {
    if (!NAME_RE.test(skill.name) || !VERSION_RE.test(version)) {
      target.textContent = "Invalid skill name or version.";
      return;
    }
    var url = RAW_BASE + "skills/" + skill.name + "/" + version + "/SKILL.md";
    target.setAttribute("data-version", version);
    if (bodyCache[url] != null) {
      target.textContent = bodyCache[url];
      return;
    }
    target.textContent = "Loading " + skill.name + " " + version + "…";
    fetch(url)
      .then(function (r) {
        if (!r.ok) throw new Error("HTTP " + r.status);
        return r.text();
      })
      .then(function (text) {
        bodyCache[url] = stripFrontmatter(text);
        if (target.getAttribute("data-version") === version)
          target.textContent = bodyCache[url];
      })
      .catch(function (err) {
        if (target.getAttribute("data-version") === version) {
          target.textContent =
            "Could not load this version (" + err.message + ").";
        }
      });
  }

  function renderSkill(skill) {
    var li = el("li", "skill-card");
    li.setAttribute("data-testid", "skill-card-" + skill.name);
    li.id = "skill-" + skill.name;

    var head = el("button", "skill-head");
    head.type = "button";
    head.setAttribute("aria-expanded", "false");
    head.setAttribute("data-testid", "skill-toggle-" + skill.name);
    var title = el("span", "skill-title");
    title.appendChild(el("span", "skill-name", skill.name));
    title.appendChild(el("span", "skill-latest", "v" + skill.latest));
    head.appendChild(title);
    head.appendChild(el("span", "skill-desc", skill.description || ""));
    li.appendChild(head);

    var meta = el("div", "skill-meta");
    var groups = skillGroups(skill);
    for (var i = 0; i < groups.length; i++)
      meta.appendChild(
        el("span", "skill-chip skill-chip-group", groupName(groups[i])),
      );
    var tags = skill.tags || [];
    for (var j = 0; j < tags.length; j++)
      meta.appendChild(el("span", "skill-chip", "#" + tags[j]));
    li.appendChild(meta);

    var panel = el("div", "skill-panel");
    panel.hidden = true;
    var bar = el("div", "skill-bar");
    var label = el("label", null, "Version ");
    var select = el("select", "skill-versions");
    select.setAttribute("data-testid", "skill-versions-" + skill.name);
    var versions = skill.versions || [skill.latest];
    for (var k = 0; k < versions.length; k++) {
      var opt = el(
        "option",
        null,
        versions[k] + (versions[k] === skill.latest ? " (latest)" : ""),
      );
      opt.value = versions[k];
      if (versions[k] === skill.latest) opt.selected = true;
      select.appendChild(opt);
    }
    label.appendChild(select);
    bar.appendChild(label);
    var sub = el("a", "skill-subscribe btn", "Subscribe in PeckBoard");
    sub.setAttribute("data-skill", skill.name);
    sub.setAttribute("data-testid", "skill-subscribe-" + skill.name);
    sub.rel = "noopener";
    sub.addEventListener("click", function (ev) {
      if (!readInstance()) {
        ev.preventDefault();
        instanceInput.focus();
        instanceStatus.textContent =
          "Enter your PeckBoard address to subscribe.";
      }
    });
    updateSubscribeLink(sub, readInstance());
    bar.appendChild(sub);
    var src = el("a", "skill-source", "View on GitHub");
    src.rel = "noopener";
    bar.appendChild(src);
    panel.appendChild(bar);
    var body = el("pre", "skill-body");
    body.setAttribute("data-testid", "skill-body-" + skill.name);
    panel.appendChild(body);
    li.appendChild(panel);

    function setSource(v) {
      src.href =
        "https://github.com/PeckBoard/skills/blob/main/skills/" +
        skill.name +
        "/" +
        v +
        "/SKILL.md";
    }
    setSource(skill.latest);

    head.addEventListener("click", function () {
      var open = panel.hidden;
      panel.hidden = !open;
      head.setAttribute("aria-expanded", String(open));
      if (open && !body.getAttribute("data-version"))
        loadBody(skill, select.value, body);
    });
    select.addEventListener("change", function () {
      setSource(select.value);
      loadBody(skill, select.value, body);
    });
    return li;
  }

  function matches(skill, terms, group) {
    if (group && skillGroups(skill).indexOf(group) < 0) return false;
    var hay = [skill.name, skill.description || ""]
      .concat(skill.tags || [])
      .join(" ")
      .toLowerCase();
    for (var i = 0; i < terms.length; i++)
      if (hay.indexOf(terms[i]) < 0) return false;
    return true;
  }

  function render() {
    var terms = qInput.value.toLowerCase().split(/\s+/).filter(Boolean);
    var group = groupSelect.value;
    var shown = index.skills.filter(function (s) {
      return matches(s, terms, group);
    });
    list.textContent = "";
    for (var i = 0; i < shown.length; i++)
      list.appendChild(renderSkill(shown[i]));
    var total = index.skills.length;
    statusEl.textContent = shown.length
      ? shown.length === total
        ? total +
          (total === 1 ? " skill" : " skills") +
          " in the public catalog"
        : shown.length + " of " + total + " skills"
      : "No skills match.";
  }

  function populateGroups() {
    var groups = index.groups.slice().sort(function (a, b) {
      return String(a.name).localeCompare(String(b.name));
    });
    for (var i = 0; i < groups.length; i++) {
      var opt = el("option", null, groups[i].name || groups[i].slug);
      opt.value = groups[i].slug;
      groupSelect.appendChild(opt);
    }
    var wanted = new URLSearchParams(window.location.search).get("group");
    if (wanted) groupSelect.value = wanted;
  }

  qInput.addEventListener("input", render);
  groupSelect.addEventListener("change", render);
  showInstance();

  var q = new URLSearchParams(window.location.search).get("q");
  if (q) qInput.value = q;

  fetch(INDEX_URL)
    .then(function (r) {
      if (!r.ok) throw new Error("HTTP " + r.status);
      return r.json();
    })
    .then(function (data) {
      index = {
        skills: Array.isArray(data && data.skills)
          ? data.skills.filter(function (s) {
              return s && NAME_RE.test(s.name) && VERSION_RE.test(s.latest);
            })
          : [],
        groups: Array.isArray(data && data.groups)
          ? data.groups.filter(function (g) {
              return g && typeof g.slug === "string";
            })
          : [],
      };
      populateGroups();
      render();
    })
    .catch(function (err) {
      statusEl.textContent =
        "Could not load the public catalog (" + err.message + "). ";
      var a = el("a", null, "Browse it on GitHub");
      a.href = "https://github.com/PeckBoard/skills";
      statusEl.appendChild(a);
      statusEl.classList.add("skills-status-error");
    });
})();
