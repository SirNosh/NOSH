import tseslint from "@typescript-eslint/eslint-plugin";
import parser from "@typescript-eslint/parser";

export default [{
  files: ["apps/{cli,noshd,tui}/src/**/*.ts", "apps/{cli,noshd,tui}/src/**/*.tsx", "packages/*/src/**/*.ts", "scripts/**/*.mjs"],
  ignores: ["**/dist/**", "**/dist-types/**", "release/**"],
  languageOptions: { parser, parserOptions: { ecmaVersion: "latest", sourceType: "module" } },
  plugins: { "@typescript-eslint": tseslint },
  rules: {
    "no-constant-binary-expression": "error",
    "no-duplicate-case": "error",
    "no-unreachable": "error",
    eqeqeq: ["error", "always"],
    "@typescript-eslint/no-unused-vars": ["error", { args: "after-used", caughtErrors: "none", ignoreRestSiblings: true }],
  },
}];
