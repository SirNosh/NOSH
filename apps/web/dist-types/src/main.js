import { jsx as _jsx } from "react/jsx-runtime";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { router } from "./App.js";
import "@fontsource-variable/geist/wght.css";
import "@fontsource-variable/geist-mono/wght.css";
import "@xterm/xterm/css/xterm.css";
import "@xyflow/react/dist/style.css";
import "./styles.css";
const queryClient = new QueryClient({ defaultOptions: { queries: { retry: 1, refetchInterval: 5_000 } } });
createRoot(document.getElementById("root")).render(_jsx(StrictMode, { children: _jsx(QueryClientProvider, { client: queryClient, children: _jsx(RouterProvider, { router: router }) }) }));
if ("serviceWorker" in navigator && import.meta.env.PROD)
    void navigator.serviceWorker.register("./sw.js");
//# sourceMappingURL=main.js.map