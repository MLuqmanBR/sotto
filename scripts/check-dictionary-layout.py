#!/usr/bin/env python3
"""Check that Dictionary rows actually lay out inside their panel.

The defect this guards: the row was a flex layout, and a global
`select { width: 100% }` rule won the cascade for the per-row case selector. The
select stretched across the row, the word column was squeezed to nothing, and the
Remove button was pushed past the panel edge, where `overflow: hidden` clipped it
away. A screenshot showed it; a unit test could not.

So this renders the **shipped** index.html, styles.css and app.js in WebKitGTK —
the same engine the app uses — in an offscreen window, then measures the real
boxes. The only thing faked is the Tauri IPC bridge.

Checks, per entry row:
  * the Remove button exists, is visible, and ends inside the panel
  * the row does not overflow horizontally
  * the case selector is a control, not a full-width bar
  * the word column keeps a usable width
  * the alias line does not run underneath the selector

Usage: scripts/check-dictionary-layout.py [--keep DIR]
Exit: 0 all pass, 1 a check failed, 2 the harness could not run.
"""

import argparse
import json
import os
import pathlib
import shutil
import sys
import tempfile

# WebKit needs a GL context it cannot get on a headless/offscreen surface here.
# These must be set before GTK initialises, which happens on the first gi call.
os.environ.setdefault("GDK_GL", "disable")
os.environ.setdefault("WEBKIT_DISABLE_COMPOSITING_MODE", "1")
os.environ.setdefault("LIBGL_ALWAYS_SOFTWARE", "1")

import gi  # noqa: E402

gi.require_version("Gtk", "3.0")
gi.require_version("WebKit2", "4.1")
from gi.repository import GLib, Gtk, WebKit2  # noqa: E402

ROOT = pathlib.Path(__file__).resolve().parent.parent
UI = ROOT / "ui"

STUB = r"""
<script>
const DICT = {
  version: 1,
  entries: [
    { id:"a", preferred:"Luqman", aliases:["Lufman"], case_policy:"exact", enabled:true },
    { id:"b", preferred:"Sotto", aliases:["Soto"], case_policy:"exact", enabled:true },
    { id:"c", preferred:"toast", aliases:["post"], case_policy:"exact", enabled:false },
    { id:"d", preferred:"Bartholomew", aliases:["Bartolome","Bartholemew","Bart"],
      case_policy:"preserve", enabled:true }
  ],
  replacements: [ {id:"r",source:"action item",target:"action point",enabled:true} ],
  duplicates: []
};
const SNAP = {
  version:"1.0.0",
  settings:{
    revision:1,
    general:{palette:"blue",theme:"dark",show_overlay:true,overlay_position:"bottom_centre",
      overlay_style:"minimal",autostart:true,close_action:"tray"},
    audio:{input_device:null,gain:1.0,mute_output:false,cues:false},
    dictation:{hold_chord:"Ctrl+Space",toggle_chord:"Ctrl+Shift+Space",cancel_chord:"Escape",
      auto_insert:true,keep_on_clipboard:false},
    dictionary:{fuzzy_enabled:false,phonetic_enabled:false,fuzzy_threshold:0.18},
    history:{retention:"three_days",max_entries:10000},
    delivery:{prefer_paste:true,restore_clipboard:true,paste_delay_ms:60},
    model:{directory:null,verify_on_start:true,idle_unload_secs:300}
  },
  model:{present:true,engine:"voxrt",kernels:"avx512-vnni",device:"cpu",directory:"/usr/lib/Sotto/model"},
  autostart:{entry_present:true,target_exists:true,target:"/usr/bin/sotto"},
  entries:4,replacements:1,history_count:0,audio_bytes:0,recording:false,level:0,
  sidecar_running:false,preview_text:"",shortcut_problems:[],
  folders:{config:"/c",data:"/d",history:"/h",audio:"/a",logs:"/l",model:"/m"}
};
window.__TAURI__ = {
  core:{ invoke: async (cmd) => {
    if (cmd === "get_snapshot") return JSON.parse(JSON.stringify(SNAP));
    if (cmd === "get_dictionary") return JSON.parse(JSON.stringify(DICT));
    if (cmd === "list_inputs") return [{name:"Built-in Audio",is_default:true}];
    if (cmd === "list_history") return [];
    if (cmd === "playback_status") return {playing:false,position_ms:0,duration_ms:0};
    return null;
  }},
  event:{ listen: async () => () => {} }
};
</script>
"""

MEASURE = r"""
(function () {
  var entries = document.querySelector('#dict-entries');
  var panel = entries.closest('.panel');
  var pr = panel.getBoundingClientRect();
  var rows = Array.prototype.slice.call(entries.querySelectorAll('.entry'));
  function box(el) { return el ? el.getBoundingClientRect() : null; }
  var out = rows.map(function (r) {
    var rb = box(r), rm = box(r.querySelector('.remove'));
    var sel = box(r.querySelector('.mini-select'));
    var main = box(r.querySelector('.entry-main'));
    var ali = box(r.querySelector('.aliases'));
    return {
      pref: (r.querySelector('.pref') || {}).textContent || '',
      overflow: r.scrollWidth - r.clientWidth,
      removeExists: !!rm,
      removeVisible: !!rm && rm.width > 0 && rm.height > 0,
      removeRight: rm ? rm.right : null,
      removeInsidePanel: !!rm && rm.right <= pr.right - 6,
      selectWidth: sel ? sel.width : -1,
      mainWidth: main ? main.width : -1,
      aliasRight: ali ? ali.right : null,
      selectLeft: sel ? sel.left : null,
      aliasClearOfSelect: !ali || !sel || ali.right <= sel.left + 0.5
    };
  });
  var ta = document.querySelector('#dict-test');
  var tcs = ta ? getComputedStyle(ta) : null;
  return JSON.stringify({
    panelRight: pr.right, panelLeft: pr.left, rows: out,
    testField: ta ? {
      overflowX: tcs.overflowX,
      overflowY: tcs.overflowY,
      needsHorizontalScroll: ta.scrollWidth > ta.clientWidth,
      boxSizing: tcs.boxSizing
    } : null
  });
})()
"""


def build_harness(dest: pathlib.Path) -> pathlib.Path:
    src = (UI / "index.html").read_text(encoding="utf-8")
    dom = src.replace(
        '<link rel="stylesheet" href="styles.css" />',
        f'<link rel="stylesheet" href="{(UI / "styles.css").as_uri()}" />',
    )
    assert dom != src, "could not rewrite the stylesheet link"
    dom = dom.replace(
        '<script src="app.js"></script>',
        STUB + f'<script src="{(UI / "app.js").as_uri()}"></script>',
    )
    assert "window.__TAURI__" in dom, "stub was not injected"
    out = dest / "harness.html"
    out.write_text(dom, encoding="utf-8")
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", default="")
    args = ap.parse_args()

    work = pathlib.Path(args.keep) if args.keep else pathlib.Path(tempfile.mkdtemp(prefix="sotto-dict-"))
    work.mkdir(parents=True, exist_ok=True)
    page = build_harness(work)

    win = Gtk.OffscreenWindow()
    win.set_default_size(760, 640)
    view = WebKit2.WebView()
    view.set_size_request(760, 640)
    win.add(view)
    win.show_all()

    result = {"data": None, "error": None}

    def fail(msg):
        result["error"] = msg
        Gtk.main_quit()

    def measure():
        def done(v, res, _data=None):
            try:
                val = view.evaluate_javascript_finish(res)
                result["data"] = val.to_string()
            except Exception as e:  # noqa: BLE001
                result["error"] = f"measure failed: {e}"
            Gtk.main_quit()

        view.evaluate_javascript(MEASURE, -1, None, None, None, done)
        return False

    def on_load(_view, event):
        if event != WebKit2.LoadEvent.FINISHED:
            return
        view.evaluate_javascript(
            "document.querySelector('.nav-item[data-page=\"dictionary\"]').click(); 'ok'",
            -1, None, None, None, None,
        )
        GLib.timeout_add(900, measure)

    view.connect("load-changed", on_load)
    view.load_uri(page.as_uri())
    GLib.timeout_add(25000, lambda: (fail("timed out"), False)[1])
    Gtk.main()

    if not args.keep:
        shutil.rmtree(work, ignore_errors=True)

    if result["error"]:
        print(f"  FAIL harness: {result['error']}")
        return 2
    if not result["data"]:
        print("  FAIL harness: no measurements returned")
        return 2

    data = json.loads(result["data"])
    rows = data["rows"]
    failures = 0

    def check(name, cond, detail=""):
        nonlocal failures
        if cond:
            print(f"  ok   {name}")
        else:
            failures += 1
            print(f"  FAIL {name}" + (f" — {detail}" if detail else ""))

    check("the dictionary rendered rows", len(rows) >= 3, f"got {len(rows)}")
    if not rows:
        print("\n  1 layout check failed.")
        return 1

    for r in rows:
        name = r["pref"] or "row"
        check(f'{name}: Remove is present and visible', r["removeVisible"], f"exists={r['removeExists']}")
        check(
            f'{name}: Remove ends inside the panel',
            r["removeInsidePanel"],
            f"button right={r['removeRight']}, panel right={data['panelRight']}",
        )
        check(f'{name}: the row does not overflow', r["overflow"] <= 1, f"overflow={r['overflow']}px")
        check(f'{name}: the case selector stays a control', 0 < r["selectWidth"] <= 190,
              f"width={r['selectWidth']:.0f}px")
        check(f'{name}: the word column keeps usable width', r["mainWidth"] >= 120,
              f"width={r['mainWidth']:.0f}px")
        check(f'{name}: aliases are clear of the selector', r["aliasClearOfSelect"],
              f"alias right={r['aliasRight']}, selector left={r['selectLeft']}")

    # The Test field. This one cannot be caught by geometry alone: under the X11
    # backend WebKit reports the empty field's content a few pixels wider than its
    # viewport, and with `overflow-x: auto` that paints a horizontal scrollbar and
    # a white scroll-corner square across the bottom of the panel. Offscreen
    # rendering does not reproduce it, so the check asserts the rule that makes
    # both backends correct rather than the symptom of one.
    field = data.get("testField")
    check("the Test field is present", field is not None)
    if field:
        check(
            "the Test field cannot paint a horizontal scrollbar",
            field["overflowX"] == "hidden",
            f"overflow-x={field['overflowX']}",
        )
        check(
            "the Test field still scrolls vertically when it must",
            field["overflowY"] == "auto",
            f"overflow-y={field['overflowY']}",
        )

    print()
    if failures:
        print(f"  {failures} layout check(s) failed.")
        return 1
    print(f"  All {len(rows)} dictionary rows lay out correctly.")
    return 0


if __name__ == "__main__":
    sys.exit(main())