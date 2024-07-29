with import <nixpkgs> {};
mkShell {
  buildInputs = [
    postgresql
  ];
}
