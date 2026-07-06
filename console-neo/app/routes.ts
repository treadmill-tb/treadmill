import {
  type RouteConfig,
  index,
  layout,
  route,
} from "@react-router/dev/routes";

export default [
  route("login", "routes/login.tsx"),
  route("login/callback", "routes/login-callback.tsx"),
  layout("layouts/shell.tsx", [index("routes/home.tsx")]),
] satisfies RouteConfig;
