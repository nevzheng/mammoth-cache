// Renders ```mermaid code blocks as diagrams. GitHub renders them natively; mdBook needs this.
(async () => {
  const blocks = document.querySelectorAll("pre > code.language-mermaid");
  if (blocks.length === 0) return;
  const { default: mermaid } = await import(
    "https://cdn.jsdelivr.net/npm/mermaid@11.4.1/dist/mermaid.esm.min.mjs"
  );
  const dark = ["coal", "navy", "ayu"].some((t) => document.documentElement.classList.contains(t));
  mermaid.initialize({ startOnLoad: false, theme: dark ? "dark" : "default" });
  for (const code of blocks) {
    const div = document.createElement("div");
    div.className = "mermaid";
    div.textContent = code.textContent;
    code.parentElement.replaceWith(div);
  }
  await mermaid.run({ querySelector: "div.mermaid" });
})();
