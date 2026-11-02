// IoT Edge's own interface at /edge/ (docs/plan/edge.md): gateways and what they reach, devices,
// alarms, robots, definitions and flows, with the rest in the records frame's lists and forms.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { StrictMode, useEffect } from "react";
import { createRoot } from "react-dom/client";
import { createBrowserRouter, Link, RouterProvider, useRouteError } from "react-router";
import { routeBase, shell } from "@ferrum/client";
import { ConfirmHost, EmptyState, ErrorState, Toaster } from "@ferrum/kit";
import { RequireAuth, setFrameMode } from "@ferrum/records/frame";
import "./styles.css";

const page = (name: "FormView" | "ListView" | "ReportView" | "AppSettingsPage" | "ScreenPage") => () => import("./records").then((m) => ({ Component: m[name] }));

const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false, staleTime: 5_000 } } });

function RouteError() {
  return <ErrorState error={useRouteError()} onRetry={() => window.location.reload()} />;
}

function NotFound() {
  return (
    <EmptyState title="Page not found">
      <Link to="/">Go to IoT Edge</Link>
    </EmptyState>
  );
}

function Started() {
  useEffect(() => shell().ready(), []);
  return null;
}

setFrameMode("app", ["edge"]);

const router = createBrowserRouter(
  [
    {
      path: "/",
      element: (
        <>
          <Started />
          <RequireAuth />
        </>
      ),
      errorElement: <RouteError />,
      children: [
        { index: true, lazy: () => import("./Home").then((m) => ({ Component: m.Home })) },
        { path: "devices", lazy: () => import("./Devices").then((m) => ({ Component: m.Devices })) },
        { path: "d/:id", lazy: () => import("./Device").then((m) => ({ Component: m.DevicePage })) },
        { path: "alarms", lazy: () => import("./Alarms").then((m) => ({ Component: m.Alarms })) },
        { path: "robots", lazy: () => import("./Robots").then((m) => ({ Component: m.Robots })) },
        { path: "definitions", lazy: () => import("./Definitions").then((m) => ({ Component: m.Definitions })) },
        { path: "definitions/:id", lazy: () => import("./Definitions").then((m) => ({ Component: m.DefinitionPage })) },
        { path: "flows", lazy: () => import("./Flows").then((m) => ({ Component: m.Flows })) },
        { path: "flows/:id", lazy: () => import("./FlowEditor").then((m) => ({ Component: m.FlowEditor })) },
        { path: "e/:entity", lazy: page("ListView") },
        { path: "e/:entity/:id", lazy: page("FormView") },
        { path: "r/:name", lazy: page("ReportView") },
        { path: "apps/:app/settings", lazy: page("AppSettingsPage") },
        { path: "x/:app/:page", lazy: page("ScreenPage") },
        { path: "*", element: <NotFound /> },
      ],
    },
  ],
  { basename: routeBase("edge") },
);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
      <Toaster />
      <ConfirmHost />
    </QueryClientProvider>
  </StrictMode>,
);
