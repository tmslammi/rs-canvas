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
    };
  }
}

export {};
