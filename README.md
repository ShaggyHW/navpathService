```sh
cargo run -p navpath-builder --release --   --sqlite ./worldReachableTiles.db   --out-snapshot ./graph.snapshot   --out-tiles ./tiles.bin  --landmarks 64

UPDATE THE PATH TO YOUR ONW

export SNAPSHOT_PATH=/home/query/Dev/navpathService/graph.snapshot 
export NAVPATH_HOST=127.0.0.1
export NAVPATH_PORT=8080
export RUST_LOG=info
cargo run -p navpath-service --release
```




THIS IS ALL AI SLOP TO PROVE A FUCKING POINT. THANK YOU FOR COMING TO MY TED TALK.

THERE WILL BE AN IMPLEMENTATION OVER AT https://gitlab.com/project-undercut/engine 

WHERE YOU'LL SEE THIS IN USE, ITS ALREADY DONE ON MY PRIVATE REPO, JUST WAITING ON APROVAL TO CREATE THE PR OVER THERE.

THE SPREADSHEET IN THE REPO IS NOT THE MAIN SPREADSHEET BUT I'LL KEEP IT UPDATED.

IF YOU HAVE NODES TO CONTRIBUTE JUST CREATE A PR FOR THIS SPREADSHEET AND I'LL ADD IT TO THE MAIN ONE AFTER VALIDATION
