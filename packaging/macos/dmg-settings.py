# dmgbuild settings for the installer window; see build-dmg.sh.
# defines: app (path to the .app), icon (volume icon .icns), background (.tiff)
import os

app = defines["app"]
app_name = os.path.basename(app)

format = "UDZO"
files = [app]
symlinks = {"Applications": "/Applications"}
background = defines["background"]
icon = defines["icon"]

show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
default_view = "icon-view"

# Frame = 32pt title bar + 400pt content + 28pt path bar (bg is 660x428 so it
# also fills the frame when the path bar is hidden). Icon y matches make-assets.py.
window_rect = ((200, 120), (660, 460))
icon_size = 112
text_size = 13
arrange_by = None
icon_locations = {app_name: (170, 185), "Applications": (490, 185)}
