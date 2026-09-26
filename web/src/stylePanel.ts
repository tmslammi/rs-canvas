interface Style {
  strokeColor: string;
  fillColor: string;
  fillStyle: string;
  strokeWidth: number;
  roughness: number;
  dash: string;
  opacity: number;
  fontSize: number;
}

export interface StyleEditor {
  current_style(): string;
  update_style(patch: string): void;
}

const STROKE_COLORS = [
  "#1e1e1e",
  "#e03131",
  "#2f9e44",
  "#1971c2",
  "#f08c00",
  "#9c36b5",
  "#0c8599",
];

const FILL_COLORS = [
  "transparent",
  "#ffec99",
  "#b2f2bb",
  "#a5d8ff",
  "#ffc9c9",
  "#d0bfff",
  "#ffd8a8",
];

export function createStylePanel(
  container: HTMLElement,
  editor: StyleEditor,
): { refresh(): void } {
  const matchers: Array<{ el: HTMLElement; match: (style: Style) => boolean }> = [];
  let opacity: HTMLInputElement;

  const row = (label: string): HTMLDivElement => {
    const section = document.createElement("div");
    section.className = "style-section";
    const title = document.createElement("div");
    title.className = "style-label";
    title.textContent = label;
    section.appendChild(title);
    const options = document.createElement("div");
    options.className = "style-options";
    section.appendChild(options);
    container.appendChild(section);
    return options;
  };

  const button = (
    parent: HTMLElement,
    glyph: string,
    title: string,
    match: (style: Style) => boolean,
    run: () => void,
  ): HTMLButtonElement => {
    const el = document.createElement("button");
    el.textContent = glyph;
    el.title = title;
    el.addEventListener("click", () => {
      run();
    });
    parent.appendChild(el);
    matchers.push({ el, match });
    return el;
  };

  const swatches = (
    label: string,
    colors: string[],
    field: "strokeColor" | "fillColor",
  ): void => {
    const options = row(label);
    for (const color of colors) {
      const el = document.createElement("button");
      el.className = "swatch";
      el.title = color === "transparent" ? `${field === "strokeColor" ? "Stroke" : "Fill"} transparent` : `${field === "strokeColor" ? "Stroke" : "Fill"} ${color}`;
      if (color === "transparent") {
        el.classList.add("swatch-transparent");
      } else {
        el.style.background = color;
      }
      el.addEventListener("click", () => {
        editor.update_style(JSON.stringify({ [field]: color }));
        refresh();
      });
      options.appendChild(el);
      matchers.push({ el, match: (style) => style[field] === color });
    }
  };

  swatches("Stroke", STROKE_COLORS, "strokeColor");
  swatches("Fill", FILL_COLORS, "fillColor");

  const fills = row("Fill style");
  const fillOptions: Array<[string, string, string]> = [
    ["▨", "Hachure", "hachure"],
    ["▩", "Cross-hatch", "crosshatch"],
    ["■", "Solid", "solid"],
  ];
  for (const [glyph, title, value] of fillOptions) {
    button(fills, glyph, `Fill style ${title}`, (style) => style.fillStyle === value, () => {
      editor.update_style(JSON.stringify({ fillStyle: value }));
      refresh();
    });
  }

  const widths = row("Stroke width");
  for (const value of [1, 2, 4]) {
    button(widths, ["·", "•", "⬤"][value === 1 ? 0 : value === 2 ? 1 : 2], `Stroke width ${value}`, (style) => style.strokeWidth === value, () => {
      editor.update_style(JSON.stringify({ strokeWidth: value }));
      refresh();
    });
  }

  const roughness = row("Roughness");
  for (const value of [0, 1, 2]) {
    button(roughness, ["◜", "◝", "◞"][value], `Roughness ${value}`, (style) => style.roughness === value, () => {
      editor.update_style(JSON.stringify({ roughness: value }));
      refresh();
    });
  }

  const dashes = row("Stroke style");
  for (const [glyph, title, value] of [
    ["—", "Solid", "solid"],
    ["– –", "Dashed", "dashed"],
    ["··", "Dotted", "dotted"],
  ] as const) {
    button(dashes, glyph, `Stroke style ${title}`, (style) => style.dash === value, () => {
      editor.update_style(JSON.stringify({ dash: value }));
      refresh();
    });
  }

  const opacityRow = row("Opacity");
  opacity = document.createElement("input");
  opacity.type = "range";
  opacity.min = "10";
  opacity.max = "100";
  opacity.value = "100";
  opacity.className = "opacity-slider";
  opacity.addEventListener("input", () => {
    editor.update_style(JSON.stringify({ opacity: Number(opacity.value) / 100 }));
  });
  opacityRow.appendChild(opacity);

  const fonts = row("Font size");
  for (const [glyph, value] of [
    ["S", 16],
    ["M", 20],
    ["L", 28],
  ] as Array<[string, number]>) {
    button(fonts, glyph, `Font size ${value}`, (style) => style.fontSize === value, () => {
      editor.update_style(JSON.stringify({ fontSize: value }));
      refresh();
    });
  }

  const refresh = (): void => {
    let style: Style;
    try {
      style = JSON.parse(editor.current_style()) as Style;
    } catch {
      return;
    }
    for (const { el, match } of matchers) {
      el.classList.toggle("active", match(style));
    }
    opacity.value = String(Math.round(style.opacity * 100));
  };

  refresh();
  return { refresh };
}
