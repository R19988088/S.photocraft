# Windows PDF renderer loading

Source: https://github.com/storytold/winprint.rs/tree/5f80b9baf6b58937c401656634e42d33f00ddf49
License: BSD-3-Clause (LICENSE.md).

This copy loads PdfCreateRenderer from System32 only when PDF printing is requested.
A missing DLL or export returns a print error instead of preventing PhotoCraft startup.
The library remains loaded until the renderer and print context have been released.
Only the two XML fixtures needed to compile the focused unit tests are included.
