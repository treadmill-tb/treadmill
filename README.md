# The Treadmill Distributed Hardware Testbed

This project is still in active development and not ready for use yet. All
information in this README is preliminary, unstable and subject to change at any
point.

## Terminology and Architecture

The Treadmill distributed testbed is composed out of multiple different
components, as outlined in this figure:

![The Treadmill distributed testbed architecture diagram, with components
explained below](./doc/img/treadmill_architecture.drawio.svg "The Treadmill
distributed testbed architecture diagram")

- A **central coordinator.**

  This is a (logically centralized) service that is responsible for
  authenticating users, scheduling jobs, managing images available in the
  system.

  It serves as the service and endpoint that users and API clients use to
  interact with the system. However, it may direct users or clients to other
  services, such as to open an interactive shell session for a job, view log
  outputs, etc.

  We want the coordinator to be lightweight and avoid storing any actual
  job-related data on it, given it being a central and trusted component in the
  system. By routing all interactions with the testbed itself through, and
  storing all job-related data (logs / images / etc.) on external services, the
  central coordinator becomes easier to maintain and scale. It also needs to be
  trusted only for correct authentication and job management, but not for
  integrity or security of other components.

- **Image store** servers.

  These components hold _images_, which are templates from which jobs can be
  created on _supervisors_ (e.g., an image for a QEMU supervisor might hold a
  base disk image).

  Image stores can hold images uploaded to them either by supervisors or
  site-local image caches, or by user-supplied images. The former enables
  snapshot-style image creation from existing jobs, whereas the latter allows
  users to inject entirely custom images into the system.

- **Log servers.**

  Supervisors can push logs of jobs to these servers, which then provide an API
  endpoint to subsequently retrieve them. Supervisors can persist logs at one or
  more servers, depending on reliability considerations.

- **Rendezvous proxy** servers.

  Treadmill does not assume that site-local services are directly reachable, as
  would be required for, e.g., interactive shell sessions onto _hosts_. To work
  around such limitations, Treadmill provides a set of _rendezvous proxy_
  servers, which allow a supervisor to expose a host-local TCP port on a
  publicly accessible TCP port or WebSocket.

- Site-local servers and services.

  Each site contains some shared infrastructure, and some components per
  individual _host_. A job is scheduled on one or more _hosts_, and each host
  may be connected to one or more boards (device under test, DUT). Each host
  should at least have the ability to program a board, and may have additional
  debug infrastructure or peripherals connected to the board.

  - **Image cache.**

    Site-local image caches function identical to image stores, with
    two main differences:

	- They may not be reachable externally. This means that it may be impossible
      to start a job at one site from an image that is only held on an image
      cache server at another site. Image caches can be requested to upload
      images to other image stores.

	- They must expose their images for supervisors to access. Most commonly,
      this will happen via NFS, a shared mount-point, or a comparable
      protocol. Supervisors can start jobs based on images held in image caches
      without holding onto images locally.

	  For instance, a QEMU virtual machine may be started using a layered
      `qcow2` disk, with some of the layers backed by an image held in the image
      cache, and exposed to the QEMU supervisor via NFS.

  - **Supervisors.**

    Supervisors manage a given host. They can manage actual physical devices
    (such as an attached Raspberry Pi), or virtual hosts (Linux containers,
    virtual machines, etc.).

	Supervisors maintain a bidirectional connection to the central
    coordinator. They report their status, receive commands (e.g., to start a
    given job), and interact with other components such as image stores, image
    caches, rendezvous proxies and log servers.

    They are responsible to ensuring isolation between jobs executing on the
    same host, such as by re-creating virtual machines or resetting the
    filesystems of an attached hardware device to the requested image state.
