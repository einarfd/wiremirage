// Bootstrap script for the slice-41 Ace Editor integration.
//
// Finds every <div data-wm-ace="..."> on the page and replaces its
// text content with an Ace editor configured per its data attributes:
//   data-wm-ace          — mode ("typescript", "javascript")
//   data-wm-ace-readonly — present → read-only viewer (no textarea sync)
//   data-wm-ace-sync     — name of a sibling <textarea> to sync into on
//                          every change. Required unless read-only.
//
// Theme follows the page's prefers-color-scheme. We listen for changes
// so users who toggle their OS theme mid-session see the editor flip
// without a reload.

(function () {
  if (typeof window === "undefined" || typeof ace === "undefined") return;

  ace.config.set("basePath", "/__ui/static/ace");

  function themeFor() {
    return window.matchMedia &&
      window.matchMedia("(prefers-color-scheme: dark)").matches
      ? "ace/theme/github_dark"
      : "ace/theme/github_light_default";
  }

  const editors = [];

  function attach(div) {
    const mode = div.dataset.wmAce || "typescript";
    const readOnly = "wmAceReadonly" in div.dataset;
    const syncName = div.dataset.wmAceSync;

    // Ace replaces the div's content, so any pre-rendered source needs
    // to come from a separate place. We support two sources, in order:
    //   1. A sibling <textarea> with the matching name (for editor mode
    //      — the form already submits this name).
    //   2. The div's textContent (for read-only viewer mode).
    let initial = div.textContent || "";
    let textarea = null;
    if (syncName) {
      textarea = document.querySelector(
        `textarea[name="${CSS.escape(syncName)}"]`,
      );
      if (textarea) initial = textarea.value;
    }

    div.textContent = ""; // Ace will populate this.
    const editor = ace.edit(div);
    editor.setOptions({
      useWorker: false, // we don't vendor worker-*.js; saves a request.
      showPrintMargin: false,
      fontSize: 14,
      tabSize: 2,
      useSoftTabs: true,
      readOnly,
      highlightActiveLine: !readOnly,
      highlightGutterLine: !readOnly,
    });
    editor.session.setMode(`ace/mode/${mode}`);
    editor.setTheme(themeFor());
    editor.setValue(initial, -1); // -1 = move cursor to start

    if (textarea && !readOnly) {
      textarea.style.display = "none";
      editor.session.on("change", () => {
        textarea.value = editor.getValue();
      });
      // Belt-and-suspenders: sync once on form submit, in case some
      // browser quirk swallows the last change event.
      const form = textarea.closest("form");
      if (form) {
        form.addEventListener("submit", () => {
          textarea.value = editor.getValue();
        });
      }
    }

    editors.push(editor);
  }

  function reTheme() {
    const t = themeFor();
    for (const e of editors) e.setTheme(t);
  }

  function init() {
    for (const div of document.querySelectorAll("[data-wm-ace]")) {
      attach(div);
    }
    if (window.matchMedia) {
      const mq = window.matchMedia("(prefers-color-scheme: dark)");
      if (mq.addEventListener) mq.addEventListener("change", reTheme);
      else if (mq.addListener) mq.addListener(reTheme);
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
