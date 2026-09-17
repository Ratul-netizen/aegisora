/**
 * The router, the query client, and the one gate between them.
 *
 * # The gate
 *
 * Every route except `/login` renders inside `Authenticated`, which fetches `/me` and
 * sends you to `/login` if that comes back 401. This is a *convenience*, not a
 * protection: the protection is that the server refuses every request without a session,
 * and nothing this file does could weaken or strengthen that. Saying so matters, because
 * a client-side guard that is mistaken for the real one is how an endpoint ends up
 * unprotected on the server "because the UI hides it".
 *
 * # Routes are defined in code
 *
 * TanStack Router's file-based routing generates a route tree into the source directory.
 * That is a build step that writes committed code, and a diff nobody reads. Six routes
 * defined here are six routes anyone can find by opening this file.
 */

import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import {
  Outlet,
  RouterProvider,
  createRootRoute,
  createRoute,
  createRouter,
  useNavigate,
} from "@tanstack/react-router";
import { StrictMode, useEffect } from "react";
import { createRoot } from "react-dom/client";

import { ApiError, api } from "./api";
import { Layout } from "./layout";
import { ExplorePage } from "./explore";
import { OverviewPage } from "./overview";
import { LoginPage } from "./pages";
import { AlertsPage, ChannelsPage, RulesPage } from "./alerts";
import { DashboardPage, DashboardsPage } from "./dashboard";
import { MapPage } from "./map";
import { ResourcePage, ResourcesPage } from "./resources";
import { ShellProvider, validateShellSearch } from "./shell";
import "./styles.css";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // A 401 means the session ended. Retrying it four times delays the redirect to
      // the login page by several seconds and cannot succeed.
      retry: (attempt, error) => !(error instanceof ApiError && error.isUnauthenticated) && attempt < 2,
      staleTime: 30_000,
      refetchOnWindowFocus: false,
    },
  },
});

/** Fetches `/me`, or sends you to sign in. */
function Authenticated() {
  const navigate = useNavigate();
  const me = useQuery({ queryKey: ["me"], queryFn: api.me });

  const unauthenticated = me.isError && me.error instanceof ApiError && me.error.isUnauthenticated;

  useEffect(() => {
    if (unauthenticated) void navigate({ to: "/login" });
  }, [unauthenticated, navigate]);

  if (me.isPending || unauthenticated) return null;

  if (me.isError) {
    return (
      <div className="empty-state">
        <h1>Cannot reach the server</h1>
        <p>{me.error instanceof ApiError ? me.error.message : String(me.error)}</p>
      </div>
    );
  }

  return (
    <ShellProvider me={me.data}>
      <Layout />
    </ShellProvider>
  );
}

const rootRoute = createRootRoute({
  // Validated once, at the root, so every route inherits the tenant and time range and
  // no route has to remember to declare them.
  validateSearch: validateShellSearch,
  component: Outlet,
});

const loginRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/login",
  component: LoginPage,
});

const shellRoute = createRoute({
  getParentRoute: () => rootRoute,
  id: "shell",
  component: Authenticated,
});

const overviewRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/",
  component: OverviewPage,
});

const resourcesRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/resources",
  component: ResourcesPage,
});

const resourceRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/resources/$id",
  component: ResourcePage,
});

const mapRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/map",
  component: MapPage,
});

const exploreRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/explore",
  component: ExplorePage,
});

const alertsRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/alerts",
  component: AlertsPage,
});

const rulesRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/alerts/rules",
  component: RulesPage,
});

const channelsRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/alerts/channels",
  component: ChannelsPage,
});

const dashboardsRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/dashboards",
  component: DashboardsPage,
});

const dashboardRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/dashboards/$id",
  component: function Dashboard() {
    // The id comes from the path; the component takes it as a prop so it can be rendered
    // in a test or a story without a router.
    const { id } = dashboardRoute.useParams();
    return <DashboardPage id={id} />;
  },
});

const routeTree = rootRoute.addChildren([
  loginRoute,
  shellRoute.addChildren([
    overviewRoute,
    mapRoute,
    resourcesRoute,
    resourceRoute,
    exploreRoute,
    alertsRoute,
    rulesRoute,
    channelsRoute,
    dashboardsRoute,
    dashboardRoute,
  ]),
]);

const router = createRouter({ routeTree });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

const root = document.getElementById("root");
if (!root) throw new Error("#root is missing from index.html");

createRoot(root).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
);
