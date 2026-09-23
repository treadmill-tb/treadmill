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
    route("jobs/new", "routes/job-new.tsx"),
    route("jobs/:id", "routes/job-detail.tsx"),
    route("hosts", "routes/hosts.tsx"),
    route("hosts/new", "routes/host-new.tsx"),
    route("hosts/:id", "routes/host-detail.tsx"),
    route("hosts/:id/spec", "routes/host-spec-edit.tsx"),
    route("images", "routes/images.tsx"),
    route("images/build/:digest", "routes/image-detail.tsx"),
    route("images/:id", "routes/image.tsx"),
    route("images/:id/edit", "routes/image-edit.tsx"),
    route("images/:id/versions/:n", "routes/image-version.tsx"),
    // Links from before image sets were called images.
    route("image-sets", "routes/image-sets-redirect.tsx", { id: "image-sets" }),
    route("image-sets/:id", "routes/image-sets-redirect.tsx", {
      id: "image-set",
    }),
    route("image-sets/:id/generations/:n", "routes/image-sets-redirect.tsx", {
      id: "image-set-generation",
    }),
    route("users/:id", "routes/user-detail.tsx"),
    route("settings", "routes/settings.tsx"),
  ]),
] satisfies RouteConfig;
