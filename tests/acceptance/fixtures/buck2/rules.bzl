"""Rules for the Buck2 fixture: one execution platform that runs actions locally and uses the
remote cache, and a rule that writes `size` bytes derived from its sources."""

def _platform_impl(ctx):
    platform = ExecutionPlatformInfo(
        label = ctx.label.raw_target(),
        configuration = ConfigurationInfo(constraints = {}, values = {}),
        executor_config = CommandExecutorConfig(
            local_enabled = True,
            remote_enabled = False,
            remote_cache_enabled = True,
            allow_cache_uploads = ctx.attrs.upload,
            use_limited_hybrid = False,
        ),
    )
    return [DefaultInfo(), ExecutionPlatformRegistrationInfo(platforms = [platform])]

platform = rule(impl = _platform_impl, attrs = {"upload": attrs.bool()})

def _blob_impl(ctx):
    out = ctx.actions.declare_output(ctx.label.name + ".out")
    script = '(yes "$(cat "$@" | sha256sum)" || true) | head -c {} > "$0"'.format(ctx.attrs.size)
    ctx.actions.run(
        ["sh", "-c", script, out.as_output()] + ctx.attrs.srcs,
        category = "blob",
        allow_cache_upload = True,
    )
    return [DefaultInfo(default_output = out)]

blob = rule(impl = _blob_impl, attrs = {
    "size": attrs.int(),
    "srcs": attrs.list(attrs.source(), default = []),
})
