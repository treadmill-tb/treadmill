interface ImportMetaEnv {
  /** Switchboard origin, e.g. `https://switchboard.example`. Empty/unset means
   * same-origin (the dev server proxies `/api`). */
  readonly VITE_TML_API_URL?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
