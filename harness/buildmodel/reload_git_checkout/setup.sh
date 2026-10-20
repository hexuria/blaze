set -e
# A registry of this machine's own with two versions of one crate: nothing is fetched.
mkdir -p registry/index/3/d registry/crates
for v in 0.1.0 0.1.1; do
  mkdir -p stage/dep-$v/src
  printf '[package]\nname = "dep"\nversion = "%s"\nedition = "2021"\ndescription = "x"\nlicense = "MIT"\n' $v > stage/dep-$v/Cargo.toml
  printf 'pub fn version() -> &'"'"'static str {\n    env!("CARGO_PKG_VERSION")\n}\n' > stage/dep-$v/src/lib.rs
  tar -czf registry/crates/dep-$v.crate -C stage dep-$v
  sum=$(sha256sum registry/crates/dep-$v.crate | cut -d' ' -f1)
  echo '{"name":"dep","vers":"'$v'","deps":[],"cksum":"'$sum'","features":{},"yanked":false}' >> registry/index/3/d/dep
done
rm -rf stage
echo '{"dl": "file://'${BM_HERE:-$PWD}'/registry/crates/{crate}-{version}.crate"}' > registry/index/config.json
(cd registry/index && git init -q && git add -A && git -c user.name=t -c user.email=t@t commit -qm index)
mkdir -p .cargo
printf '[registries.local]\nindex = "file://%s/registry/index"\n' "${BM_HERE:-$PWD}" > .cargo/config.toml
cargo generate-lockfile --quiet
cargo update --quiet -p dep --precise 0.1.0

printf 'registry/\ntarget*/\n.cargo/\nsetup.sh\nnext/\n' > .gitignore
git init -q -b one
git add -A && git -c user.name=t -c user.email=t@t commit -qm one
git checkout -q -b two
cp -r next/. . && rm app/src/gone.rs && git rm -q -r --cached old > /dev/null && rm -rf old
cargo update --quiet -p dep --precise 0.1.1
git add -A && git -c user.name=t -c user.email=t@t commit -qm two
git checkout -q one
