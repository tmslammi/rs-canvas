declare global {
  interface Window {
    __rsReady?: boolean;
    __rsAgentConnected?: boolean;
    __rsEditor?: {
      scroll(): Float64Array;
      zoom(): number;
      screen_to_world(x: number, y: number): Float64Array;
      zoom_to_fit(): void;
      reset_zoom(): void;
      element_count(): number;
      pointer_down(x: number, y: number, button: number, mods: { shift: boolean; alt: boolean; ctrl: boolean; meta: boolean }): void;
      pointer_move(x: number, y: number, mods: { shift: boolean; alt: boolean; ctrl: boolean; meta: boolean }): void;
      pointer_up(x: number, y: number, button: number, mods: { shift: boolean; alt: boolean; ctrl: boolean; meta: boolean }): void;
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
      commit_text(id: string, text: string): void;
      save_json(): string;
      save_bytes(): Uint8Array;
      snapshot_json(): string;
      load_json(json: string): void;
      load_bytes(bytes: Uint8Array): void;
      flush_save(): void;
      export_svg(): string;
      export_png(scale: number): string;
      load_benchmark_fixture(count: number): void;
      load_connected_fixture(nodes: number, arrowsPerNode: number): void;
      insert_glyph(glyphId: string): void;
      is_tool_locked(): boolean;
      set_grid_visible(visible: boolean): void;
      grid_visible(): boolean;
      set_grid_size(size: number): void;
      grid_size(): number;
      set_zoom_sensitivity(sensitivity: number): void;
      zoom_sensitivity(): number;
      set_snap_enabled(enabled: boolean): void;
      snap_enabled(): boolean;
      route_for(id: string): Float64Array;
      wheel(x: number, y: number, dx: number, dy: number, ctrl: boolean): void;
      zoom_at(x: number, y: number, factor: number): void;
      render(): void;
      benchmark_tick(dx: number, dy: number): void;
      presentation_state(): string;
      presentation_outcomes(): string;
    };
  }
}

export {};
