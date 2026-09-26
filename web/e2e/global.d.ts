declare global {
  interface Window {
    __rsReady?: boolean;
    __rsEditor?: {
      scroll(): Float64Array;
      zoom(): number;
      screen_to_world(x: number, y: number): Float64Array;
      zoom_to_fit(): void;
      reset_zoom(): void;
      element_count(): number;
      new_document(): void;
      active_tool(): string;
      set_tool(name: string): void;
      can_undo(): boolean;
      can_redo(): boolean;
      undo(): void;
      redo(): void;
      selection_bounds(): Float64Array | undefined;
      current_style(): string;
      update_style(patch: string): void;
      save_json(): string;
      load_json(json: string): void;
    };
  }
}

export {};
