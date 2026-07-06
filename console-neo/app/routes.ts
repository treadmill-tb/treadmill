import {
  type RouteConfig,
  index,
  layout,
  route,
} from "@react-router/dev/routes";

export default [
  route("login", "routes/login.tsx"),
  route("login/callback", "routes/login-callback.tsx"),
  layout("layouts/shell.tsx", [
    index("routes/home.tsx"),
    route("jobs", "routes/jobs.tsx"),
    route("jobs/:id", "routes/job-detail.tsx"),
    route("hosts", "routes/hosts.tsx"),
    route("hosts/:id", "routes/host-detail.tsx"),
    route("images", "routes/images.tsx"),
    route("images/:digest", "routes/image-detail.tsx"),
    route("image-groups", "routes/image-groups.tsx"),
    route("image-groups/:id", "routes/image-group-detail.tsx"),
    route("image-groups/:id/generations/:n", "routes/generation-detail.tsx"),
    route("users/:id", "routes/user-detail.tsx"),
    route("settings", "routes/settings.tsx"),
  ]),
] satisfies RouteConfig;
