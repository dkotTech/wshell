import { render } from 'preact';
import '../css/style.css';
import { Drawer } from '../components/Drawer';
import { AppTable, ModalHost, Summary, Toasts, Toolbar, TopBar } from '../components/Layout';
import { startPolling } from '../store';

function App() {
  return (
    <>
      <TopBar />
      <main>
        <Summary />
        <Toolbar />
        <AppTable />
      </main>
      <Drawer />
      <ModalHost />
      <Toasts />
    </>
  );
}

render(<App />, document.getElementById('app')!);
startPolling();
