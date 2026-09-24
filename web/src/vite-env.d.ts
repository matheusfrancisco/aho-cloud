/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Control-plane origin when it is not the console's own (e.g. `https://api.aho.cloud`). */
  readonly VITE_AHO_CLOUD_API?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
