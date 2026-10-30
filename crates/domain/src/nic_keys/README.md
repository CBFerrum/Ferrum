# NIC's keys for signed e-invoice QR codes

Published by NIC at https://einvoice1.gst.gov.in/Others/Publickeys ("E-Invoice Public Keys for
verification of signed content"), fetched on 2026-10-30 from `https://einvoice1.gst.gov.in//Documents/<file>`.
Each archive holds the key (`.pem`) and its signing certificate (`.cer`). A key is stored here under
its certificate's SHA-1 thumbprint, which is the `kid` NIC's signed tokens name (NIC's own sample in
its generate-IRN API documentation names `115F4426…EDAB` and verifies with that key). Each key was
checked to be its certificate's and re-encoded with openssl; nothing else from the archives is kept.
NIC publishes new keys before old ones lapse (the current ones run to 01/05/2027): add the next ones
here, and until then a company can enter NIC's current key as its IRP QR key.

| File | Archive | Archive SHA-256 | Signs |
|---|---|---|---|
| `ProdPubKey3.pem` | `ProdPubKey3.zip` | `8cfb5c11f58a78712265671c4355a8d49bb9309ecfeb6ecb62e49f3dbd888660` | e-Invoice1 production, IRNs of 28/09/2020 to 16/09/2022 (published without its certificate) |
| `CD01AC2B6E720CF0B9E048F1CCBC7509C766AF78.pem` | `einv1publickeyApr2027.zip` | `08e1025a17096e6471309bf853f688dd0e2f45a538c41da05cbbcdedc5cfb6e9` | e-Invoice1 production, IRNs of 03/06/2026 to 01/05/2027 |
| `6C9E76A15241484CBBFC0CB81A5F3F0E5D489918.pem` | `einv2publickey1.zip` | `66c7169dfc69cc811c5967c3a7aa8c522a164db45a47a3de99cbafea5215b25c` | e-Invoice2 production, IRNs of 01/04/2023 to 04/09/2024 |
| `DEB2031A279F203BBDDF3538E77D4122ACB4D07D.pem` | `einv2publickey2024.zip` | `40ca4713a397e715663554f37757ec1f2d12fd58fd5be82be6f4394bf3b83819` | e-Invoice2 production, IRNs of 05/09/2024 to 03/06/2026 |
| `644956C46E4E947AAD72FD15DFDB01EB9A96A66B.pem` | `einv2publickeyApr2027.zip` | `b444e2d60f131dc65fc6b0f070042b26da523359f294900750fd973a4eb24855` | e-Invoice2 production, IRNs of 03/06/2026 to 01/05/2027 |
| `B8D63ECB58EA5E6F4AAC3D524543B2246669B0F8.pem` | `einvpublickey4.zip` | `c6858724ece1136478a12261e533d65df8b8faad7507c4588aec9b054becf9c3` | e-Invoice1 production, IRNs of 16/09/2022 to 06/09/2024 |
| `4DE1544AE695BDC84EC7BC12F2F57F813F44E301.pem` | `einvpublickey5.zip` | `ac0a89ad4f9ff070d18fbfbb6b352ec4f328915083f997807232c72bb09d26a8` | e-Invoice1 production, IRNs of 06/09/2024 to 03/06/2026 |
| `115F4426617A7938BE1BA06DBEE91A427584EDAB.pem` | `prodpubkey1.zip` | `ba6493a00d9191d191abbf3be829126a8256840f50c13c630323ac13f93b9a74` | e-Invoice1 production, IRNs of 05/08/2020 to 11/09/2020 (and the sandbox until 18/09/2020) |
| `247EBBCBE931277CD0BCCEB00CB021D98E886DAC.pem` | `prodpubkey2.zip` | `a71162821a8c5d05468c30e818fd4dba608cdc9bd7efec7ff218f0db0ef95b19` | e-Invoice1 production, IRNs of 11/09/2020 to 28/09/2020 |
| `EDC57DE1358B300BA9F79340A6696831F3C85047.pem` | `sandpubkey2.zip` | `4608540ea5f6090cbea2e46f160619cb873893ac4a0199ddeb460b4a9101c6e4` | sandbox, IRNs from 18/09/2020 |
