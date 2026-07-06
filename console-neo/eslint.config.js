import reactHooks from "eslint-plugin-react-hooks";
import tseslint from "typescript-eslint";

export default tseslint.config(
  { ignores: ["build/", ".react-router/", "app/api/schema.d.ts"] },
  tseslint.configs.recommended,
  reactHooks.configs.flat.recommended,
);
