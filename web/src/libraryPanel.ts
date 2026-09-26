interface GlyphInfo {
  id: string;
  name: string;
  category: string;
}

export interface LibraryEditor {
  insert_glyph(id: string): void;
}

const CATEGORY_LABELS: Record<string, string> = {
  compute: "Compute",
  storage: "Storage & Data",
  networking: "Networking",
  devops: "DevOps",
  ai: "AI / ML",
  security: "Security",
  observability: "Observability",
  generic: "Generic",
};

const CATEGORY_ORDER = [
  "compute",
  "storage",
  "networking",
  "devops",
  "ai",
  "security",
  "observability",
  "generic",
];

export function createLibraryPanel(
  container: HTMLElement,
  editor: LibraryEditor,
  catalogJson: string,
): { refresh(): void } {
  let catalog: GlyphInfo[] = [];
  try {
    catalog = JSON.parse(catalogJson) as GlyphInfo[];
  } catch {
    catalog = [];
  }

  const search = document.createElement("input");
  search.type = "search";
  search.placeholder = "Search shapes";
  search.className = "library-search";
  container.appendChild(search);

  const list = document.createElement("div");
  list.className = "library-list";
  container.appendChild(list);

  const render = (): void => {
    const query = search.value.trim().toLowerCase();
    list.replaceChildren();
    for (const category of CATEGORY_ORDER) {
      const glyphs = catalog.filter(
        (glyph) =>
          glyph.category === category &&
          (query === "" ||
            glyph.name.toLowerCase().includes(query) ||
            glyph.id.includes(query)),
      );
      if (glyphs.length === 0) continue;
      const section = document.createElement("div");
      section.className = "library-section";
      const title = document.createElement("div");
      title.className = "library-label";
      title.textContent = CATEGORY_LABELS[category] ?? category;
      section.appendChild(title);
      for (const glyph of glyphs) {
        const button = document.createElement("button");
        button.textContent = glyph.name;
        button.title = `Insert ${glyph.name}`;
        button.addEventListener("click", () => {
          editor.insert_glyph(glyph.id);
        });
        section.appendChild(button);
      }
      list.appendChild(section);
    }
  };

  search.addEventListener("input", render);
  render();

  return { refresh: render };
}
