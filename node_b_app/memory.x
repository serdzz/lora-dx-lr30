MEMORY
{
  /* Active-app region inside the bootloader's flash layout — see
     ../swap-common/src/layout.rs. This is NOT "all of flash starting at
     0x08000000"; the first 4 KB (0x08000000-0x08000FFF) belong to
     ../bootloader, and everything from 0x08010800 onward is the OTA
     staging region this binary must not touch (its own linked code lives
     entirely below that boundary — ACTIVE_LEN enforces it). */
  FLASH : ORIGIN = 0x08001000, LENGTH = 62K
  RAM   : ORIGIN = 0x20000000, LENGTH = 20K
}
