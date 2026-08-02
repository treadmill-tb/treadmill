interface ImportMetaEnv {
  /** Switchboard origin, e.g. `https://switchboard.example`. Empty/unset means
   * same-origin (the dev server proxies `/api`). */
  readonly VITE_TML_API_URL?: string;

  /** Revision this console was built from. */
  readonly VITE_TML_CONSOLE_REV?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
