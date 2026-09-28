# .NET + Chromium acceptance on Docker Desktop — TDD ledger

Phase 7 of the revised plan. The goal is the whole feature on the real
daemon. Pi builds and runs a containerized ASP.NET Core app through the
broker, and the Chromium sidecar opens it by its generated host name. There
are no production code changes in this phase; the earlier phases already
support the flow.

## Evidence

- **Fake boundary, characterization:**
  `managed_services::app_and_sidecar_share_the_run_network_and_only_the_viewer_is_published`.
  - The sidecar and the app join the same run network.
  - The sidecar publishes only `127.0.0.1::6080`; the app publishes nothing.
  - The app has its `pithos-app-<hash>` alias.
  - Cleanup removes both containers, then the network.
  - It passed on first run, so it is characterization, not a Red. The
    combination was already correct.
- **Real Docker Desktop:**
  `broker_real_docker::docker_desktop_pi_builds_a_dotnet_app_and_chromium_browses_it`
  passes in 43 s. The .NET 8 SDK image was cached; ASP.NET 8 was pulled.
  - Workspace: `src/Api/{Api.csproj, Program.cs, Dockerfile}`, a multi-stage
    SDK → aspnet build listening on 8080. The browser is enabled.
  - Inside the managed Pi container, the *mounted* extension builds the app
    (`src/Api/Dockerfile`, context `src/Api`) and runs it. It retries HTTP
    until Kestrel answers (`ready true`). The app runs as 65532 on a read-only
    root with no changes to the app.
  - Host-side `docker inspect` of every managed container:
    - `app-…`: `{}`;
    - `runtime-pi-v1`: `{}`;
    - `runtime-browser-v1`: only `127.0.0.1` on 6080.

    No host port exists for the app.
  - `pithos-browser` in Pi: `goto http://pithos-app-<hash>:8080/` gives the
    title `pithos dotnet`, and the snapshot shows the heading
    `hello from aspnet`. The screenshot `dotnet.png` has the PNG magic bytes.
  - Logs contain Kestrel's `Now listening on`. Stop, then status, report
    `stopped`.
  - The viewer returns 200 on loopback and the password file exists. The run
    settles `Complete` with no managed container or network left.
- **Not covered:** a model driving this through Pi's chat. Pi itself was
  checked in phase 6: its loader lists the mounted extension. The tool calls
  here are the same extension code with the same credential, called
  directly.
