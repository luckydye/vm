# Screen and input

`vm ui` opens a window with the VMs of this machine and every node. It starts, stops, creates and deletes them, and shows the selected VM's screen with mouse, keyboard, sound and clipboard. On Linux it needs a Wayland session. The sidebar shows the selected VM's kind, CPU, memory, disk and display, plus a running VM's addresses and listening ports; click a port to open it in the browser or forward it to this computer's localhost. `⌘B` (`Ctrl+B` on Linux) hides the sidebar. Its light and dark themes follow the host's appearance, in its own chrome, its terminals and the VM's Hyprland desktop, and its pointer follows the shape the VM shows.

## From the CLI

Coordinates are screenshot pixels:

```sh
vm screenshot $vm shot.png
vm screen $vm                 # use it live in the terminal, with mouse and keyboard
vm move $vm 640 400
vm click $vm 640 400 [--right | --middle] [--double]
vm type $vm "hello world"
vm key $vm ctrl+alt+t enter
vm scroll $vm -3
vm stream $vm | my-viewer      # the screen as H.264 and the sound as Opus, for other programs
my-driver | vm input $vm       # mouse and keyboard as JSON lines
```

`vm screen` takes the terminal's aspect ratio and follows it when the window is resized. Press `Ctrl+]` to detach.

## Graphics

Graphics are hardware accelerated through the host GPU:

```text
guest Vulkan => Mesa Venus => virglrenderer => MoltenVK => Metal           (macOS)
guest OpenGL => Mesa VirGL => virglrenderer => ANGLE    => Metal           (macOS)
guest Vulkan => Mesa Venus => virglrenderer => host Vulkan driver          (Linux)
guest OpenGL => Mesa VirGL => virglrenderer => host OpenGL driver over EGL (Linux)
```
