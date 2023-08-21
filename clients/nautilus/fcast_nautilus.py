import os
import gi
import json
import mimetypes
import socket
import logging
from netifaces import interfaces, ifaddresses, AF_INET  
from contextlib import closing
import http.server
import subprocess
import sys
from gi.repository import GLib

#log_file = os.path.join(os.path.expanduser('~'), 'fcast.log')
#logging.basicConfig(filename=log_file, level=logging.DEBUG, format='%(asctime)s - %(levelname)s - %(message)s')
logging.info("Logging system initialized.") 

gi.require_version('Nautilus', '3.0')
gi.require_version('Soup', '3.0')
from gi.repository import Nautilus, GObject, Gtk, Gio, Soup

class CustomHTTPRequestHandler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, file_path, *args, **kwargs):
        self.file_path = file_path
        super().__init__(*args, **kwargs)

    def do_GET(self):
        self.send_file(True)

    def do_HEAD(self):
        self.send_file(False)

    def send_file(self, send_body):
        logging.debug(f"Preparing to send file: {self.file_path}.")

        try:
            self.file_size = os.stat(self.file_path).st_size

            # Check for partial download headers.
            range_header = self.headers.get('Range', None)
            if not range_header:
                self.send_response(200)
                start_byte = 0
                end_byte = self.file_size - 1
            else:
                start_byte, end_byte = self.parse_range_header(range_header)
                self.send_response(206)

            # Dynamically determine the media type of the file
            media_type, _ = mimetypes.guess_type(self.file_path)
            if not media_type:
                # If the media type couldn't be determined, default to 'application/octet-stream'
                media_type = 'application/octet-stream'

            self.send_header('Content-Type', media_type)
            self.send_header('Content-Disposition', f'attachment; filename="{os.path.basename(self.file_path)}"')
            self.send_header('Content-Range', f"bytes {start_byte}-{end_byte}/{self.file_size}")
            self.send_header('Content-Length', str(end_byte - start_byte + 1))
            self.end_headers()

            if send_body:
                # Stream the file
                with open(self.file_path, 'rb') as f:
                    f.seek(start_byte)
                    self.copy_file_range(f, self.wfile, start_byte, end_byte)

        except Exception as e:
            logging.error(f"Error while sending file: {str(e)}")
            self.send_error(500, str(e))

    def parse_range_header(self, range_header):
        logging.info(f"Parsing range header: {range_header}.")

        # Expects a HTTP range header string and returns the two numbers as tuple
        start_byte, end_byte = range_header.split('=')[1].split('-')
        start_byte = int(start_byte.strip())
        end_byte = int(end_byte.strip()) if end_byte else self.file_size - 1
        return start_byte, end_byte

    def copy_file_range(self, input_file, output_file, start=0, end=None):
        logging.info(f"Copying file range from {start} to {end}.")

        bytes_to_send = end - start + 1
        buffer_size = 8192  # 8KB buffer
        bytes_sent = 0
        while bytes_sent < bytes_to_send:
            buffer = input_file.read(min(buffer_size, bytes_to_send - bytes_sent))
            if not buffer:
                break  # EOF reached
            output_file.write(buffer)
            bytes_sent += len(buffer)

def run_http_server(ip, port, path):
    try:
        # Set the working directory to the directory of the file
        os.chdir(os.path.dirname(path))
        
        # Start the server
        handler = lambda *args, **kwargs: CustomHTTPRequestHandler(path, *args, **kwargs)
        httpd = http.server.HTTPServer((ip, port), handler)
        logging.info(f"Web server started on port '{ip}:{port}'.")

        # Get the local IP address
        logging.info(f"Local IP {ip}.")

        httpd.serve_forever()
    except Exception as e:
        logging.error(f"Error in HTTP server: {e}")

    logging.info("Stopped HTTP server.")

class FCastMenuProvider(GObject.GObject, Nautilus.MenuProvider):
    def __init__(self):
        self.selected_file_path = None
        logging.info("Initialized FCastMenuProvider.")

    def get_file_items(self, window, files):
        if len(files) != 1:
            return

        self.selected_file_path = files[0].get_location().get_path()
        item = Nautilus.MenuItem(name="NautilusPython::fcast_item",
                                 label="Cast To",
                                 tip="Cast selected file")
        submenu = Nautilus.Menu()
        item.set_submenu(submenu)
        
        hosts = self.load_hosts()
        for host in hosts:
            host_item = Nautilus.MenuItem(name=f"NautilusPython::fcast_sub_item_{host}",
                                          label=host,
                                          tip=f"Cast to {host}")
            host_item.connect('activate', self.cast_to_host, host)
            submenu.append_item(host_item)

        add_item = Nautilus.MenuItem(name="NautilusPython::fcast_add_host",
                                     label="Add New Host",
                                     tip="Add a new host")
        add_item.connect('activate', self.add_host)
        submenu.append_item(add_item)
        
        return [item]

    def load_hosts(self):
        config_path = os.path.join(os.path.expanduser('~'), '.fcast_hosts.json')
        logging.info(f"Loading hosts from {config_path}.")
        if not os.path.exists(config_path):
            logging.warning(f"Config file {config_path} does not exist.")
            return []
        
        with open(config_path, 'r') as f:
            hosts = json.load(f)
        
        if not isinstance(hosts, list):
            logging.error(f"Hosts data is not a list: {hosts}.")
            return []
        
        logging.info(f"Loaded hosts: {hosts}.")
        return hosts

    def save_host(self, host):
        logging.debug(f"Saving host: {host}.")

        hosts = self.load_hosts()
        if host not in hosts:
            hosts.append(host)
            config_path = os.path.join(os.path.expanduser('~'), '.fcast_hosts.json')
            with open(config_path, 'w') as f:
                json.dump(hosts, f)

    def add_host(self, menu_item):
        logging.info(f"Adding host.")

        dialog = Gtk.Dialog(title="Add Host", parent=None, flags=0)
        host_entry = Gtk.Entry(placeholder_text="Enter Host IP e.g. 192.168.1.1")
        port_entry = Gtk.Entry(placeholder_text="Enter Port e.g. 46899")

        box = dialog.get_content_area()
        box.add(host_entry)
        box.add(port_entry)

        dialog.add_buttons(Gtk.STOCK_CANCEL, Gtk.ResponseType.CANCEL, Gtk.STOCK_OK, Gtk.ResponseType.OK)
        dialog.connect('response', self.on_add_host_response, host_entry, port_entry)
        dialog.show_all()

    def on_add_host_response(self, dialog, response, host_entry, port_entry):
        logging.debug("Received response from add host dialog.")

        if response == Gtk.ResponseType.OK:
            host = host_entry.get_text()
            port = port_entry.get_text()
            if host and port:
                self.save_host(f"{host}:{port}")
        dialog.destroy()

    def cast_to_host(self, menu_item, host_data):
        logging.info(f"Attempting to cast to {host_data} with file {self.selected_file_path}.")

        host, port = host_data.split(":")
        mimetype, _ = mimetypes.guess_type(self.selected_file_path)
        if not mimetype:
            logging.error(f"Could not determine MIME type for file: {self.selected_file_path}")
            return

        local_url = self.start_web_server(self.selected_file_path, host)
        logging.info(f"Started web server with local URL: {local_url}.")

        def callback():
            self.notify_fcast_server(host, port, local_url, mimetype)

        GLib.timeout_add_seconds(1, callback)

    def get_local_ip_address(self, target_ip):
        logging.info(f"Getting local IP address suitable for target IP {target_ip}.")

        # Extract the subnet from the target IP (assuming a typical subnet mask like 255.255.255.0)
        subnet = ".".join(target_ip.split('.')[:-1])

        # Get all the network interfaces and their associated IP addresses
        ip_list = []
        for ifaceName in interfaces():
            addresses = [i['addr'] for i in ifaddresses(ifaceName).setdefault(AF_INET, [{'addr':'No IP addr'}])]
            ip_list.extend(addresses)

        # Filter IPs based on the subnet
        ip_list = [ip for ip in ip_list if ip.startswith(subnet)]

        if len(ip_list) == 1:
            return ip_list[0]
        elif len(ip_list) > 1:
            # If there are multiple IPs, ask the user to choose one
            dialog = Gtk.Dialog(title="Select IP Address", parent=None, flags=0)
            combo = Gtk.ComboBoxText()
            for ip in ip_list:
                combo.append_text(ip)
            combo.set_active(0)
            box = dialog.get_content_area()
            box.add(combo)
            dialog.add_buttons(Gtk.STOCK_CANCEL, Gtk.ResponseType.CANCEL, Gtk.STOCK_OK, Gtk.ResponseType.OK)
            dialog.show_all()
            response = dialog.run()
            selected_ip = combo.get_active_text()
            dialog.destroy()
            if response == Gtk.ResponseType.OK:
                return selected_ip
        else:
            logging.error("No valid IP address found!")
            return None
        
    def find_free_port(self):
        logging.debug("Finding a free port.")

        with closing(socket.socket(socket.AF_INET, socket.SOCK_STREAM)) as s:
            s.bind(('', 0))
            s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            return s.getsockname()[1]

    def start_web_server(self, file_path, target_ip):
        logging.info(f"Starting the web server for file {file_path}.")

        local_port = self.find_free_port()
        logging.info(f"Found free port {local_port}.")

        local_ip = self.get_local_ip_address(target_ip)
        
        # Use subprocess to spawn a new process running the server
        script_directory = os.path.dirname(os.path.abspath(__file__))
        command = [sys.executable, '-c', 'import sys; sys.path.append("{}"); from fcast_nautilus import run_http_server; run_http_server("{}", {}, "{}")'.format(script_directory, local_ip, local_port, file_path)]
        subprocess.Popen(command, start_new_session=True, cwd=script_directory)

        if local_ip:
            return f"http://{local_ip}:{local_port}/"
        else:
            logging.error("Unable to determine local IP address.")
            return None

    def notify_fcast_server(self, host, port, url, mimetype):
        logging.info(f"Attempting to notify the fcast server at {host}:{port} with URL {url}.")

        # Create the message
        message = {
            "container": mimetype,
            "url": url
        }
        json_data = json.dumps(message).encode('utf-8')

        # Print JSON
        logging.info(f"Send JSON to ({host}:${port}): {json_data}.")

        # Create the header
        opcode = 1  # Play opcode
        length = len(json_data) + 1  # 1 for opcode
        length_bytes = length.to_bytes(4, 'little')
        opcode_byte = opcode.to_bytes(1, 'little')

        # Construct the packet
        packet = length_bytes + opcode_byte + json_data

        # Send the packet using a TCP socket
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.connect((host, int(port)))
            s.sendall(packet)