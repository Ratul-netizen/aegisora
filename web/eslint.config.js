import js from "@eslint/js";
import globals from "globals";
import reactHooks from "eslint-plugin-react-hooks";
import tseslint from "typescript-eslint";

export default tseslint.config(
  { ignores: ["dist"] },
  {
    extends: [js.configs.recommended, ...tseslint.configs.recommended],
    files: ["src/**/*.{ts,tsx}"],
    languageOptions: { ecmaVersion: 2022, globals: globals.browser },
    plugins: { "react-hooks": reactHooks },
    rules: {
      ...reactHooks.configs.recommended.rules,
      // The session lives in an HttpOnly cookie the browser sends on its own. Nothing
      // in this app should be reaching for storage, and a lint is cheaper than noticing
      // in review.
      "no-restricted-globals": [
        "error",
        { name: "localStorage", message: "no credential or tenant state in storage; see src/api.ts" },
        { name: "sessionStorage", message: "no credential or tenant state in storage; see src/api.ts" },
      ],
    },
  },
);
