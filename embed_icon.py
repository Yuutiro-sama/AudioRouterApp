import struct
import ctypes
from ctypes import wintypes

# Win32 Constants
RT_ICON = 3
RT_GROUP_ICON = 14

kernel32 = ctypes.WinDLL('kernel32', use_last_error=True)

BeginUpdateResourceW = kernel32.BeginUpdateResourceW
BeginUpdateResourceW.argtypes = [wintypes.LPCWSTR, wintypes.BOOL]
BeginUpdateResourceW.restype = wintypes.HANDLE

UpdateResourceW = kernel32.UpdateResourceW
UpdateResourceW.argtypes = [
    wintypes.HANDLE,
    wintypes.LPCWSTR,
    wintypes.LPCWSTR,
    wintypes.WORD,
    wintypes.LPVOID,
    wintypes.DWORD
]
UpdateResourceW.restype = wintypes.BOOL

EndUpdateResourceW = kernel32.EndUpdateResourceW
EndUpdateResourceW.argtypes = [wintypes.HANDLE, wintypes.BOOL]
EndUpdateResourceW.restype = wintypes.BOOL

def embed_ico_into_exe(exe_path, ico_path, icon_id=1):
    with open(ico_path, 'rb') as f:
        ico_data = f.read()

    # Parse ICO Header (ICONDIR)
    idReserved, idType, idCount = struct.unpack_from('<HHH', ico_data, 0)
    if idType != 1:
        raise ValueError("Not a valid ICO file (idType != 1)")

    hUpdate = BeginUpdateResourceW(exe_path, False)
    if not hUpdate:
        err = ctypes.get_last_error()
        raise RuntimeError(f"BeginUpdateResourceW failed with error {err}")

    # Build GRPICONDIR
    grp_dir = struct.pack('<HHH', 0, 1, idCount)
    entries = []
    offset = 6

    for i in range(idCount):
        bWidth, bHeight, bColorCount, bReserved, wPlanes, wBitCount, dwBytesInRes, dwImageOffset = struct.unpack_from(
            '<BBBBHHII', ico_data, offset
        )
        offset += 16
        raw_image_data = ico_data[dwImageOffset : dwImageOffset + dwBytesInRes]
        
        # Each image gets its own RT_ICON resource with ID = icon_id + i
        res_id = icon_id + i
        
        # Write RT_ICON
        res_type = ctypes.cast(ctypes.c_void_p(RT_ICON), wintypes.LPCWSTR)
        res_name = ctypes.cast(ctypes.c_void_p(res_id), wintypes.LPCWSTR)
        buf = (ctypes.c_char * len(raw_image_data)).from_buffer_copy(raw_image_data)
        
        ok = UpdateResourceW(
            hUpdate,
            res_type,
            res_name,
            0, # Language Neutral
            buf,
            len(raw_image_data)
        )
        if not ok:
            err = ctypes.get_last_error()
            EndUpdateResourceW(hUpdate, True)
            raise RuntimeError(f"UpdateResourceW(RT_ICON, {res_id}) failed: {err}")

        # GRPICONDIRENTRY
        grp_entry = struct.pack(
            '<BBBBHHIH',
            bWidth, bHeight, bColorCount, bReserved,
            wPlanes, wBitCount, dwBytesInRes, res_id
        )
        entries.append(grp_entry)

    # Write RT_GROUP_ICON
    group_icon_data = grp_dir + b''.join(entries)
    res_type = ctypes.cast(ctypes.c_void_p(RT_GROUP_ICON), wintypes.LPCWSTR)
    # The group name can be ID 1 (or "MAINICON")
    res_name = ctypes.cast(ctypes.c_void_p(icon_id), wintypes.LPCWSTR)
    buf = (ctypes.c_char * len(group_icon_data)).from_buffer_copy(group_icon_data)

    ok = UpdateResourceW(
        hUpdate,
        res_type,
        res_name,
        0, # Language Neutral
        buf,
        len(group_icon_data)
    )
    if not ok:
        err = ctypes.get_last_error()
        EndUpdateResourceW(hUpdate, True)
        raise RuntimeError(f"UpdateResourceW(RT_GROUP_ICON) failed: {err}")

    # Commit changes
    ok = EndUpdateResourceW(hUpdate, False)
    if not ok:
        err = ctypes.get_last_error()
        raise RuntimeError(f"EndUpdateResourceW commit failed: {err}")

    print(f"Successfully embedded {idCount} icon resolutions into {exe_path}!")

if __name__ == '__main__':
    import sys
    exe = sys.argv[1] if len(sys.argv) > 1 else 'AudioRouter.exe'
    ico = sys.argv[2] if len(sys.argv) > 2 else 'app.ico'
    embed_ico_into_exe(exe, ico)
