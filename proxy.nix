{ writeShellApplication, python3 }:

writeShellApplication {
  name = "claude-proxy";
  runtimeInputs = [ python3 ];
  text = ''exec python3 ${./auth-proxy.py} "$@"'';
}
