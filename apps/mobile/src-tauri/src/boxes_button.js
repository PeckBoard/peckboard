// App-owned "Boxes" button on a box page: back to the app's box list.
// Evaluated by the app after each box page load (`nav::boxes_button_script`
// fills in the two arguments at the bottom); a plain navigation, no IPC. The
// shell stops the tunnel when it loads. Sits over the box UI's rail logo
// (decoration only), themed with the box UI's own colour variables, with
// light/dark fallbacks. Styled through CSSOM only: the box's CSP
// (`style-src 'self'`) blocks a `<style>` element or `style` attribute.
(function (origin, home) {
  if (location.origin !== origin || window.top !== window) return;
  if (document.getElementById("__pbm_boxes")) return;

  function css(el, props) {
    for (var k in props) el.style.setProperty(k, props[k]);
  }
  var dark = matchMedia("(prefers-color-scheme: dark)");
  var small = matchMedia("(max-width: 480px)");

  var btn = document.createElement("button");
  btn.id = "__pbm_boxes";
  btn.type = "button";
  btn.title = "Boxes";
  btn.setAttribute("aria-label", "Boxes");
  btn.innerHTML = [
    '<span><svg width="20" height="20" viewBox="0 0 24 24" fill="none"',
    ' stroke="currentColor" stroke-width="2" stroke-linecap="round"',
    ' stroke-linejoin="round" aria-hidden="true"><path d="M7 7l-4 5 4 5"/>',
    '<rect x="10" y="6.5" width="4.5" height="4.5" rx="1"/>',
    '<rect x="16.5" y="6.5" width="4.5" height="4.5" rx="1"/>',
    '<rect x="10" y="13" width="4.5" height="4.5" rx="1"/>',
    '<rect x="16.5" y="13" width="4.5" height="4.5" rx="1"/></svg></span>',
  ].join("");
  var chip = btn.firstChild;
  css(btn, {
    all: "initial",
    position: "fixed",
    "box-sizing": "border-box",
    width: "44px",
    height: "44px",
    display: "flex",
    "align-items": "center",
    "justify-content": "center",
    cursor: "pointer",
    "touch-action": "manipulation",
    "-webkit-tap-highlight-color": "transparent",
    color: "var(--text, var(--pbm-fg))",
  });
  css(chip, {
    all: "initial",
    "box-sizing": "border-box",
    display: "flex",
    "align-items": "center",
    "justify-content": "center",
    width: "34px",
    height: "34px",
    "border-radius": "9px",
    border: "1px solid var(--border-strong, var(--pbm-bd))",
    color: "inherit",
    "box-shadow": "0 1px 2px rgba(0, 0, 0, 0.12)",
  });
  css(chip.firstChild, { display: "block" });

  function pressed(on) {
    css(chip, {
      background: on
        ? "var(--surface-hover, var(--pbm-bg))"
        : "var(--surface, var(--pbm-bg))",
    });
  }
  function theme() {
    css(btn, {
      "--pbm-bg": dark.matches ? "#1a1d27" : "#fff",
      "--pbm-fg": dark.matches ? "#e5e7eb" : "#1a1d23",
      "--pbm-bd": dark.matches ? "#3a3d48" : "#d1d5db",
    });
  }
  pressed(false);
  theme();
  [dark, small].forEach(function (q) {
    var f = q === dark ? theme : tick;
    if (q.addEventListener) q.addEventListener("change", f);
    else q.addListener(f);
  });
  addEventListener("resize", function () {
    tick();
  });
  btn.addEventListener("pointerdown", function () {
    pressed(true);
  });
  ["pointerup", "pointercancel", "pointerleave"].forEach(function (t) {
    btn.addEventListener(t, function () {
      pressed(false);
    });
  });
  btn.addEventListener("focus", function () {
    if (btn.matches(":focus-visible"))
      css(chip, {
        outline: "2px solid var(--ring, currentColor)",
        "outline-offset": "2px",
      });
  });
  btn.addEventListener("blur", function () {
    chip.style.removeProperty("outline");
  });
  btn.addEventListener("click", function (ev) {
    ev.preventDefault();
    ev.stopPropagation();
    // Replace, so Back from the box list can't land on this box after its
    // tunnel is gone.
    location.replace(home);
  });

  // "Relayed" badge: the app sets `data-relayed` (`nav::relay_badge_script`)
  // after this script and on every tunnel status. Inside the 44px target,
  // across the chip's lower edge, so it covers nothing more of the box UI.
  var badge = document.createElement("span");
  badge.textContent = "Relayed";
  badge.setAttribute("aria-hidden", "true");
  css(badge, {
    all: "initial",
    position: "absolute",
    left: "50%",
    bottom: "0px",
    transform: "translateX(-50%)",
    display: "none",
    padding: "0 4px",
    "border-radius": "6px",
    font: "700 8px/12px -apple-system, system-ui, sans-serif",
    "white-space": "nowrap",
    "pointer-events": "none",
    background: "var(--warning, #f59e0b)",
    color: "#1a1d23",
    "box-shadow": "0 0 0 1.5px var(--surface, var(--pbm-bg))",
  });
  btn.appendChild(badge);
  function relay() {
    var on = btn.getAttribute("data-relayed") === "1";
    css(badge, { display: on ? "block" : "none" });
    btn.setAttribute(
      "aria-label",
      on ? "Boxes — connected via relay" : "Boxes",
    );
    btn.title = on
      ? "Boxes. Your connection goes through relay.peckboard.com " +
        "(end-to-end encrypted) because a direct path isn't available " +
        "on this network."
      : "Boxes";
  }
  new MutationObserver(relay).observe(btn, {
    attributes: true,
    attributeFilter: ["data-relayed"],
  });

  // Above the rail, below the box UI's overlays — except over a modal's
  // empty backdrop (e.g. the sign-in dialog), so the way out is there
  // before signing in, without covering a tall dialog's own content.
  function lift() {
    var r = btn.getBoundingClientRect();
    var clear = !!document.querySelector(".modal-backdrop");
    document.querySelectorAll(".modal").forEach(function (m) {
      var b = m.getBoundingClientRect();
      if (
        b.left < r.right &&
        b.right > r.left &&
        b.top < r.bottom &&
        b.bottom > r.top
      )
        clear = false;
    });
    btn.style.setProperty("z-index", clear ? "2147483000" : "25");
  }

  // Keep the button attached and centred on the box UI's rail logo (top bar
  // on phones, side rail on wide screens; the rail mounts after sign-in),
  // and on small phones leave room for its whole target before the first
  // rail button. No rail: top-left, clear of the notch.
  function tick() {
    if (!btn.isConnected)
      (document.body || document.documentElement).appendChild(btn);
    var brand = document.querySelector(".rail-brand");
    if (brand) {
      brand.style.marginRight = small.matches ? "10px" : "";
      var b = brand.getBoundingClientRect();
      css(btn, {
        top: Math.max(0, b.top + b.height / 2 - 22) + "px",
        left: Math.max(0, b.left + b.width / 2 - 22) + "px",
      });
    } else {
      css(btn, {
        top: "calc(env(safe-area-inset-top, 0px) + 6px)",
        left: "max(6px, env(safe-area-inset-left, 0px))",
      });
    }
    lift();
  }
  tick();
  new MutationObserver(tick).observe(
    document.body || document.documentElement,
    {
      childList: true,
    },
  );
  document.addEventListener("scroll", lift, true);
  setInterval(tick, 1000);
})("__ORIGIN__", "__HOME__");
